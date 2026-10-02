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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn parse_accepts_valid_arguments_without_repair() {
        let parsed = parse_tool_arguments(r#"{"command":"echo hi","timeout_secs":5}"#)
            .expect("valid JSON parses");
        assert!(!parsed.repaired);
        assert_eq!(parsed.value["command"], json!("echo hi"));
        assert_eq!(parsed.value["timeout_secs"], json!(5));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn parse_reports_invalid_tool_arguments() {
        let error = parse_tool_arguments("{not json").expect_err("unrepairable input");
        assert!(error.starts_with("invalid tool arguments:"), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
