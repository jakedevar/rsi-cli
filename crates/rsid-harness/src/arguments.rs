//! Repair malformed tool-call argument JSON and apply bounded schema coercion.
//!
//! The Harness provider is rsid's own direct-API agent loop. Models sometimes
//! emit near-valid argument JSON (a raw control character inside a string, an
//! invalid escape, or a trailing comma) or a wrongly typed scalar (a numeric
//! string where the schema wants a number). Both used to fail as an unrelated
//! "missing field" error because malformed arguments silently became `{}`.
//!
//! Repair and coercion are adapted from PrimeIntellect-ai/prime-agent
//! `packages/ai/src/utils/json-parse.ts` and
//! `packages/ai/src/utils/validation.ts` @ cd1f215c (MIT); see
//! `THIRD_PARTY_NOTICES.md`.
//!
//! After coercion, [`validate_with_schema`] checks the arguments against the
//! tool's schema and [`invalid_arguments_message`] turns every problem into
//! one error the model can fix in a single retry (#2257). That validator is
//! RSI's own, not a port.

use serde_json::{Map, Value};

/// Note appended to a tool result when its argument JSON had to be repaired.
pub const REPAIR_NOTE: &str = "[note: malformed tool-argument JSON was repaired before dispatch]";

/// Parsed tool arguments plus whether the raw text had to be repaired.
#[derive(Debug)]
pub struct ParsedToolArguments {
    pub value: Value,
    pub repaired: bool,
}

/// Parse a tool call's raw argument string, repairing near-valid JSON.
///
/// Returns a specific `invalid tool arguments: ...` error when neither the raw
/// nor the repaired text parses, so the model sees a parse error rather than a
/// downstream schema "missing field" error.
pub fn parse_tool_arguments(raw: &str) -> Result<ParsedToolArguments, String> {
    let error = match serde_json::from_str::<Value>(raw) {
        Ok(value) => {
            return Ok(ParsedToolArguments {
                value,
                repaired: false,
            });
        }
        Err(error) => error,
    };

    let repaired = repair_json(raw);
    if repaired != raw
        && let Ok(value) = serde_json::from_str::<Value>(&repaired)
    {
        return Ok(ParsedToolArguments {
            value,
            repaired: true,
        });
    }

    Err(format!("invalid tool arguments: {error}"))
}

/// Repair near-valid JSON text without changing its meaning.
///
/// Inside string literals: escape raw control characters, double backslashes
/// before invalid escape sequences, and keep well-formed `\uXXXX` escapes.
/// Outside string literals: drop a trailing comma before `}` or `]` (an RSI
/// addition; upstream leaves trailing commas alone).
pub fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];

        if !in_string {
            if current == ',' && is_trailing_comma(&chars, index) {
                index += 1;
                continue;
            }
            repaired.push(current);
            if current == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        if current == '"' {
            repaired.push(current);
            in_string = false;
            index += 1;
            continue;
        }

        if current == '\\' {
            match chars.get(index + 1).copied() {
                None => {
                    repaired.push_str("\\\\");
                    index += 1;
                }
                Some('u') => {
                    if let Some(digits) = unicode_escape_digits(&chars, index) {
                        repaired.push_str("\\u");
                        repaired.push_str(&digits);
                        index += 6;
                    } else {
                        repaired.push_str("\\\\");
                        index += 1;
                    }
                }
                Some(next) if is_valid_escape(next) => {
                    repaired.push('\\');
                    repaired.push(next);
                    index += 2;
                }
                Some(_) => {
                    repaired.push_str("\\\\");
                    index += 1;
                }
            }
            continue;
        }

        if is_control_character(current) {
            repaired.push_str(&escape_control_character(current));
        } else {
            repaired.push(current);
        }
        index += 1;
    }
    repaired
}

/// The four hex digits of a well-formed `\uXXXX` escape starting at `slash`.
fn unicode_escape_digits(chars: &[char], slash: usize) -> Option<String> {
    let digits: String = chars.get(slash + 2..slash + 6)?.iter().collect();
    (digits.len() == 4 && digits.chars().all(|digit| digit.is_ascii_hexdigit())).then_some(digits)
}

/// Whether the comma at `index` is the final token before `}` or `]`.
fn is_trailing_comma(chars: &[char], index: usize) -> bool {
    let mut next = index + 1;
    while next < chars.len() && chars[next].is_whitespace() {
        next += 1;
    }
    matches!(chars.get(next), Some('}') | Some(']'))
}

const fn is_valid_escape(candidate: char) -> bool {
    matches!(candidate, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')
}

const fn is_control_character(candidate: char) -> bool {
    (candidate as u32) <= 0x1f
}

fn escape_control_character(candidate: char) -> String {
    match candidate {
        '\u{08}' => "\\b".to_string(),
        '\u{0c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        other => format!("\\u{:04x}", other as u32),
    }
}

/// Apply bounded, schema-directed coercion to parsed tool arguments.
///
/// Only meaning-preserving scalar coercions are applied: a numeric string to
/// `number`/`integer`, and the exact strings `"true"`/`"false"` to `boolean`.
/// Every other value is left untouched, so a repair never guesses a value that
/// changes meaning.
pub fn coerce_with_schema(value: &mut Value, schema: &Value) {
    coerce_value(value, schema);
}

fn coerce_value(value: &mut Value, schema: &Value) {
    let Some(schema) = schema.as_object() else {
        return;
    };
    let types = schema_types(schema);

    if let Some(coerced) = coerce_primitive(value, &types) {
        *value = coerced;
    }

    if types.iter().any(|declared| declared == "object") {
        coerce_object(value, schema);
    }

    if types.iter().any(|declared| declared == "array")
        && let Some(items) = schema.get("items")
        && let Value::Array(array) = value
    {
        for item in array.iter_mut() {
            coerce_value(item, items);
        }
    }
}

fn coerce_object(value: &mut Value, schema: &Map<String, Value>) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let Value::Object(object) = value else {
        return;
    };
    for (key, property_schema) in properties {
        if let Some(field) = object.get_mut(key) {
            coerce_value(field, property_schema);
        }
    }
}

/// Coerce one scalar by its declared type, if exactly one listed type applies.
fn coerce_primitive(value: &Value, types: &[String]) -> Option<Value> {
    if types.len() > 1
        && types
            .iter()
            .any(|declared| matches_json_type(value, declared))
    {
        return None;
    }
    types
        .iter()
        .find_map(|declared| coerce_by_type(value, declared))
}

fn coerce_by_type(value: &Value, declared: &str) -> Option<Value> {
    match declared {
        "number" => number_from_string(value),
        "integer" => integer_from_string(value),
        "boolean" => boolean_from_string(value),
        _ => None,
    }
}

fn number_from_string(value: &Value) -> Option<Value> {
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    let parsed: f64 = text.parse().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    serde_json::Number::from_f64(parsed).map(Value::Number)
}

fn integer_from_string(value: &Value) -> Option<Value> {
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    let parsed: f64 = text.parse().ok()?;
    if !parsed.is_finite() || parsed.fract() != 0.0 {
        return None;
    }
    if parsed < i64::MIN as f64 || parsed > i64::MAX as f64 {
        return None;
    }
    Some(Value::Number((parsed as i64).into()))
}

fn boolean_from_string(value: &Value) -> Option<Value> {
    match value.as_str()? {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        _ => None,
    }
}

/// JSON Schema `type` names the validator understands. A schema naming any
/// other type is not checked, so a vendor extension never rejects a call.
const KNOWN_TYPES: [&str; 7] = [
    "string", "number", "integer", "boolean", "array", "object", "null",
];

/// Schema keywords that make a node's accepted shape depend on more than its
/// `type`; such a node is left to the tool, never guessed at.
const COMPOSITE_KEYWORDS: [&str; 6] = ["oneOf", "anyOf", "allOf", "not", "$ref", "if"];

/// Most problems listed in one error, so a hopeless call does not flood the
/// model's context.
const MAX_LISTED_PROBLEMS: usize = 12;

/// Longest scalar echoed back in a "got ..." clause.
const MAX_ECHOED_CHARS: usize = 40;

/// Longest tool signature in the "Expected:" line.
const MAX_SIGNATURE_CHARS: usize = 600;

/// Check parsed (and already coerced) tool arguments against the tool's JSON
/// Schema and return every problem found, so the model can fix them in one
/// call instead of one error per retry (#2257).
///
/// Checked: the `type` of the arguments and of every listed property (nested
/// objects and array items included), `required` fields and `enum` values.
/// Not checked: bounds, lengths, patterns and extra fields; those stay with
/// the tool, which clamps or explains them itself. A `null` for an optional
/// field counts as omitted, and a `null` argument object as an empty one, as
/// the tools already read them. A node using `oneOf`/`anyOf`/`allOf`/`not`/
/// `$ref`/`if`, or naming a type outside [`KNOWN_TYPES`], is not checked.
pub fn validate_with_schema(value: &Value, schema: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let empty = Value::Object(Map::new());
    let value = if value.is_null() { &empty } else { value };
    validate_node(value, schema, "", &mut problems);
    problems
}

fn validate_node(value: &Value, schema: &Value, path: &str, problems: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else {
        return;
    };
    if COMPOSITE_KEYWORDS
        .iter()
        .any(|keyword| schema.contains_key(*keyword))
    {
        return;
    }
    let types = schema_types(schema);
    if !types.is_empty() {
        if types
            .iter()
            .any(|declared| !KNOWN_TYPES.contains(&declared.as_str()))
        {
            return;
        }
        if !types
            .iter()
            .any(|declared| matches_json_type(value, declared))
        {
            problems.push(format!(
                "{}: expected {}, got {}",
                field_label(path),
                types.join(" or "),
                describe_value(value)
            ));
            return;
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.is_empty()
        && !allowed.contains(value)
    {
        problems.push(format!(
            "{}: must be one of {}, got {}",
            field_label(path),
            enum_choices(allowed),
            describe_value(value)
        ));
        return;
    }
    match value {
        Value::Object(object) => validate_object(object, schema, path, problems),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|items| items.is_object()) {
                for (index, item) in items.iter().enumerate() {
                    validate_node(item, item_schema, &format!("{path}[{index}]"), problems);
                }
            }
        }
        _ => {}
    }
}

fn validate_object(
    object: &Map<String, Value>,
    schema: &Map<String, Value>,
    path: &str,
    problems: &mut Vec<String>,
) {
    let properties = schema.get("properties").and_then(Value::as_object);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for name in &required {
        if !object.contains_key(*name) {
            let expected = properties
                .and_then(|properties| properties.get(*name))
                .map_or_else(|| "a value".to_string(), type_hint);
            problems.push(format!(
                "missing required field `{}` (expected {expected})",
                child_path(path, name)
            ));
        }
    }
    let Some(properties) = properties else {
        return;
    };
    for (name, property_schema) in properties {
        let Some(field) = object.get(name) else {
            continue;
        };
        if field.is_null() && !required.contains(&name.as_str()) {
            continue;
        }
        validate_node(field, property_schema, &child_path(path, name), problems);
    }
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}.{name}")
    }
}

fn field_label(path: &str) -> String {
    if path.is_empty() {
        "the arguments".to_string()
    } else {
        format!("`{path}`")
    }
}

fn describe_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => format!("boolean {flag}"),
        Value::Number(number) => format!("number {number}"),
        Value::String(text) => {
            let mut echoed: String = text.chars().take(MAX_ECHOED_CHARS).collect();
            if text.chars().count() > MAX_ECHOED_CHARS {
                echoed.push('…');
            }
            format!("string {echoed:?}")
        }
        Value::Array(items) => format!("an array of {} item(s)", items.len()),
        Value::Object(_) => "an object".to_string(),
    }
}

fn enum_choices(allowed: &[Value]) -> String {
    let choices: Vec<String> = allowed.iter().map(Value::to_string).collect();
    format!("[{}]", choices.join(", "))
}

/// A short human type for a property schema: `string`, `integer`,
/// `"a"|"b"`, `array<string>`.
fn type_hint(schema: &Value) -> String {
    let Some(schema) = schema.as_object() else {
        return "a value".to_string();
    };
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.is_empty()
    {
        return allowed
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("|");
    }
    let types = schema_types(schema);
    if types.is_empty() {
        return "a value".to_string();
    }
    let item_hint = schema
        .get("items")
        .filter(|items| items.is_object())
        .map(type_hint);
    types
        .iter()
        .map(|declared| match (declared.as_str(), item_hint.as_deref()) {
            ("array", Some(item)) if item != "a value" => format!("array<{item}>"),
            _ => declared.clone(),
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// `tool(path: string, offset?: integer)`: required fields first, then the
/// optional ones, so a model sees the whole call shape at once.
fn signature(tool: &str, schema: &Value) -> Option<String> {
    let properties = schema.get("properties")?.as_object()?;
    if properties.is_empty() {
        return Some(format!("{tool}()"));
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::with_capacity(properties.len());
    for name in required
        .iter()
        .filter(|name| properties.contains_key(**name))
    {
        parts.push(format!("{name}: {}", type_hint(&properties[*name])));
    }
    for (name, property_schema) in properties {
        if !required.contains(&name.as_str()) {
            parts.push(format!("{name}?: {}", type_hint(property_schema)));
        }
    }
    let mut text = format!("{tool}({})", parts.join(", "));
    if text.chars().count() > MAX_SIGNATURE_CHARS {
        text = text.chars().take(MAX_SIGNATURE_CHARS).collect();
        text.push('…');
    }
    Some(text)
}

/// The tool error for arguments that failed [`validate_with_schema`]: every
/// problem, the fields the tool does not take (they usually name the field the
/// model meant), the tool's call shape, and what to do next. The tool was not
/// run.
pub fn invalid_arguments_message(
    tool: &str,
    args: &Value,
    schema: &Value,
    problems: &[String],
) -> String {
    let mut message = format!("invalid arguments for tool `{tool}`; it was not run:");
    for problem in problems.iter().take(MAX_LISTED_PROBLEMS) {
        message.push_str("\n- ");
        message.push_str(problem);
    }
    if problems.len() > MAX_LISTED_PROBLEMS {
        message.push_str(&format!(
            "\n- ... and {} more",
            problems.len() - MAX_LISTED_PROBLEMS
        ));
    }
    if let (Some(object), Some(properties)) = (
        args.as_object(),
        schema.get("properties").and_then(Value::as_object),
    ) {
        let unknown: Vec<String> = object
            .keys()
            .filter(|key| !properties.contains_key(*key))
            .map(|key| format!("`{key}`"))
            .collect();
        if !unknown.is_empty() && !properties.is_empty() {
            let known: Vec<&str> = properties.keys().map(String::as_str).collect();
            message.push_str(&format!(
                "\nThis tool takes no field {}; its fields are: {}.",
                unknown.join(", "),
                known.join(", ")
            ));
        }
    }
    if let Some(signature) = signature(tool, schema) {
        message.push_str("\nExpected: ");
        message.push_str(&signature);
    }
    message.push_str("\nFix the arguments and call the tool again.");
    message
}

fn schema_types(schema: &Map<String, Value>) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(single)) => vec![single.clone()],
        Some(Value::Array(declared)) => declared
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn matches_json_type(value: &Value, declared: &str) -> bool {
    match declared {
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_accepts_valid_arguments_without_repair() {
        let parsed = parse_tool_arguments(r#"{"command":"echo hi","timeout_secs":5}"#)
            .expect("valid JSON parses");
        assert!(!parsed.repaired);
        assert_eq!(parsed.value["command"], json!("echo hi"));
        assert_eq!(parsed.value["timeout_secs"], json!(5));
    }

    #[test]
    fn parse_repairs_trailing_comma_and_control_characters() {
        let trailing = parse_tool_arguments(r#"{"command":"echo hi",}"#).expect("trailing comma");
        assert!(trailing.repaired);
        assert_eq!(trailing.value["command"], json!("echo hi"));

        let control =
            parse_tool_arguments("{\"command\":\"echo\nhi\"}").expect("control character");
        assert!(control.repaired);
        assert_eq!(control.value["command"], json!("echo\nhi"));

        let bad_escape = parse_tool_arguments(r#"{"pattern":"\d+"}"#).expect("invalid escape");
        assert!(bad_escape.repaired);
        assert_eq!(bad_escape.value["pattern"], json!("\\d+"));
    }

    #[test]
    fn parse_reports_invalid_tool_arguments() {
        let error = parse_tool_arguments("{not json").expect_err("unrepairable input");
        assert!(error.starts_with("invalid tool arguments:"), "{error}");
    }

    #[test]
    fn coerce_applies_only_the_listed_scalar_coercions() {
        let schema = json!({
            "type": "object",
            "properties": {
                "timeout_secs": {"type": "integer"},
                "ratio": {"type": "number"},
                "stream": {"type": "boolean"},
                "command": {"type": "string"}
            }
        });

        let mut args = json!({
            "timeout_secs": "30",
            "ratio": "1.5",
            "stream": "true",
            "command": "echo hi"
        });
        coerce_with_schema(&mut args, &schema);
        assert_eq!(args["timeout_secs"], json!(30));
        assert_eq!(args["ratio"], json!(1.5));
        assert_eq!(args["stream"], json!(true));
        assert_eq!(args["command"], json!("echo hi"));
    }

    fn read_file_schema() -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "minimum": 1},
                "mode": {"type": "string", "enum": ["lines", "bytes"]}
            }
        })
    }

    #[test]
    fn validate_lists_every_missing_and_invalid_field() {
        let schema = read_file_schema();
        let problems = validate_with_schema(
            &json!({"offset": "soon", "limit": 2.5, "mode": "words"}),
            &schema,
        );
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("missing required field `path` (expected string)"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`offset`: expected integer, got string \"soon\""))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`limit`: expected integer, got number 2.5"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`mode`: must be one of [\"lines\", \"bytes\"]"))
        );
    }

    #[test]
    fn validate_accepts_what_the_tools_already_accept() {
        let schema = read_file_schema();
        // Optional nulls are omissions, whole numbers are integers, and extra
        // or bounds-violating fields are the tool's business.
        assert!(validate_with_schema(&json!({"path": "a", "offset": null}), &schema).is_empty());
        assert!(validate_with_schema(&json!({"path": "a", "limit": 3.0}), &schema).is_empty());
        assert!(
            validate_with_schema(&json!({"path": "a", "offset": 0, "extra": 1}), &schema)
                .is_empty()
        );
        // A null argument object is an empty one: fine for a no-argument tool.
        let no_args = json!({"type": "object", "properties": {}});
        assert!(validate_with_schema(&Value::Null, &no_args).is_empty());
        // But not for a tool with required fields.
        assert_eq!(validate_with_schema(&Value::Null, &schema).len(), 1);
    }

    #[test]
    fn validate_reaches_nested_objects_and_array_items() {
        let schema = json!({
            "type": "object",
            "required": ["plan"],
            "properties": {"plan": {"type": "array", "items": {
                "type": "object",
                "required": ["step", "status"],
                "properties": {"step": {"type": "string"}, "status": {"type": "string"}}
            }}}
        });
        let problems = validate_with_schema(
            &json!({"plan": [{"step": "a", "status": "done"}, {"step": 7}]}),
            &schema,
        );
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`plan[1].step`: expected string"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("missing required field `plan[1].status`"))
        );
        let wrong_shape = validate_with_schema(&json!({"plan": "do it"}), &schema);
        assert_eq!(
            wrong_shape,
            vec!["`plan`: expected array, got string \"do it\""]
        );
    }

    #[test]
    fn validate_leaves_unmodelled_schema_nodes_to_the_tool() {
        let composite = json!({
            "type": "object",
            "properties": {"target": {"oneOf": [{"type": "string"}, {"type": "integer"}]}}
        });
        assert!(validate_with_schema(&json!({"target": true}), &composite).is_empty());
        let vendor = json!({"type": "object", "properties": {"x": {"type": "any"}}});
        assert!(validate_with_schema(&json!({"x": [1]}), &vendor).is_empty());
        assert!(validate_with_schema(&json!({"x": 1}), &json!({})).is_empty());
        let union = json!({"type": "object", "properties": {"x": {"type": ["string", "null"]}}});
        assert!(validate_with_schema(&json!({"x": null}), &union).is_empty());
        assert_eq!(validate_with_schema(&json!({"x": 3}), &union).len(), 1);
    }

    #[test]
    fn invalid_arguments_message_names_the_fix() {
        let schema = read_file_schema();
        let args = json!({"file_path": "a.rs", "limit": "ten"});
        let problems = validate_with_schema(&args, &schema);
        let message = invalid_arguments_message("read_file", &args, &schema, &problems);
        assert!(message.starts_with("invalid arguments for tool `read_file`; it was not run:"));
        assert!(message.contains("- missing required field `path` (expected string)"));
        assert!(message.contains("- `limit`: expected integer, got string \"ten\""));
        assert!(message.contains("This tool takes no field `file_path`; its fields are: "));
        // Required fields come first in the call shape, then the optional ones.
        assert!(
            message.contains("\nExpected: read_file(path: string, "),
            "{message}"
        );
        for optional in [
            "limit?: integer",
            "offset?: integer",
            "mode?: \"lines\"|\"bytes\"",
        ] {
            assert!(message.contains(optional), "{message}");
        }
        assert!(message.ends_with("Fix the arguments and call the tool again."));
    }

    #[test]
    fn invalid_arguments_message_bounds_a_long_problem_list() {
        let properties: serde_json::Map<String, Value> = (0..20)
            .map(|index| (format!("f{index:02}"), json!({"type": "string"})))
            .collect();
        let required: Vec<String> = properties.keys().cloned().collect();
        let schema = json!({"type": "object", "required": required, "properties": properties});
        let args = json!({});
        let problems = validate_with_schema(&args, &schema);
        assert_eq!(problems.len(), 20);
        let message = invalid_arguments_message("many", &args, &schema, &problems);
        assert_eq!(message.matches("\n- ").count(), MAX_LISTED_PROBLEMS + 1);
        assert!(message.contains("... and 8 more"));
    }

    #[test]
    fn coerce_never_changes_meaning() {
        let schema = json!({
            "type": "object",
            "properties": {
                "timeout_secs": {"type": "integer"},
                "ratio": {"type": "number"},
                "stream": {"type": "boolean"},
                "command": {"type": "string"}
            }
        });

        let mut args = json!({
            "timeout_secs": "not a number",
            "ratio": "   ",
            "stream": "yes",
            "command": "true"
        });
        coerce_with_schema(&mut args, &schema);
        assert_eq!(args["timeout_secs"], json!("not a number"));
        assert_eq!(args["ratio"], json!("   "));
        assert_eq!(args["stream"], json!("yes"));
        assert_eq!(args["command"], json!("true"));

        let mut fractional = json!({"timeout_secs": "3.5"});
        coerce_with_schema(&mut fractional, &schema);
        assert_eq!(fractional["timeout_secs"], json!("3.5"));
    }
}
