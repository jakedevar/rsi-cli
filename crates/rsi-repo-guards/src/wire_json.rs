//! Shared legacy JSON bytes for runner selection and QA identities. Extracted
//! from #363's tested c80a8762f primitives so integration can re-export these
//! from qa_fingerprint rather than keep two serializer implementations.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

pub fn sha256(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

/// `json.dumps(sort_keys=True, ensure_ascii=True)`, including the original
/// compact fingerprint separators or default cache-file separators. Do not use
/// serde's UTF-8 serializer here: non-ASCII host/class labels change the hash.
pub fn legacy_json(value: &Value, compact: bool) -> String {
    fn string(value: &str) -> String {
        let mut out = String::from("\"");
        for ch in value.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                ' '..='~' => out.push(ch),
                _ => {
                    for unit in ch.encode_utf16(&mut [0; 2]) {
                        let _ = write!(out, "\\u{unit:04x}");
                    }
                }
            }
        }
        out.push('"');
        out
    }
    let comma = if compact { "," } else { ", " };
    let colon = if compact { ":" } else { ": " };
    match value {
        Value::String(s) => string(s),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|v| legacy_json(v, compact))
                .collect::<Vec<_>>()
                .join(comma)
        ),
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|k| format!("{}{colon}{}", string(k), legacy_json(&values[k], compact)))
                    .collect::<Vec<_>>()
                    .join(comma)
            )
        }
        Value::Number(n) if n.is_f64() => {
            // CPython repr uses scientific notation below 1e-4 and at 1e16,
            // always signs and pads the exponent, and retains integral .0.
            let value = n.as_f64().unwrap_or(0.0);
            let scientific = format!("{value:e}");
            let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
            let exponent = exponent.parse::<i32>().unwrap_or(0);
            if !(-4..16).contains(&exponent) {
                format!("{mantissa}e{exponent:+03}")
            } else {
                let mut result = value.to_string();
                if !result.contains('.') {
                    result.push_str(".0");
                }
                result
            }
        }
        _ => value.to_string(),
    }
}
