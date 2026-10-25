//! Legacy v1 wire primitives for the sweep fingerprint: CPython-compatible
//! JSON bytes, sha256 hex and the QA host class. Kept local so rsi-cloud (a
//! production dependency of rsid) never depends on rsi-repo-guards (#451,
//! #4001); the bytes match that crate's `wire_json` and `qa_fingerprint`.
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

pub fn sha256(bytes: impl AsRef<[u8]>) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes.as_ref())
        .as_ref()
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// `json.dumps(sort_keys=True, ensure_ascii=True)` with the compact
/// fingerprint separators or the default ones. Do not use serde's UTF-8
/// serializer: non-ASCII host/class labels change the hash.
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

/// Only stdout participates in version probes. Stderr may contain rustup
/// progress or warnings, and a failed probe must never produce a fingerprint.
pub fn output(repo: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let done = Command::new(program)
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !done.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        ));
    }
    String::from_utf8(done.stdout)
        .map(|s| s.trim().to_owned())
        .map_err(|e| e.to_string())
}

fn host_class_from(label: &str, hostname: &str, machine_id: &str) -> Result<String, String> {
    let hostname = hostname.trim().to_lowercase();
    if hostname.is_empty() {
        return Err("QA host has no hostname".into());
    }
    let identity = sha256(format!("{hostname}\0{}", machine_id.trim()));
    Ok(format!("{label}@{hostname}:{}", &identity[..16]))
}

pub fn host_class(cwd: &Path) -> Result<String, String> {
    let label = match std::env::var("RSI_QA_HOST_CLASS")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(label) => label,
        None => {
            #[cfg(unix)]
            let (system, machine) = (
                output(cwd, "uname", &["-s"])?,
                output(cwd, "uname", &["-m"])?,
            );
            #[cfg(windows)]
            let (system, machine) = (
                "Windows".to_owned(),
                std::env::var("PROCESSOR_ARCHITEW6432")
                    .or_else(|_| std::env::var("PROCESSOR_ARCHITECTURE"))
                    .unwrap_or_else(|_| std::env::consts::ARCH.to_owned()),
            );
            #[cfg(not(any(unix, windows)))]
            return Err("QA host identity is unsupported on this platform".into());
            #[cfg(any(unix, windows))]
            format!("{}-{}", system.to_lowercase(), machine.to_lowercase())
        }
    };
    let hostname = output(cwd, "hostname", &[])?;
    #[cfg(target_os = "linux")]
    let machine_id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
    #[cfg(not(target_os = "linux"))]
    let machine_id = String::new();
    host_class_from(&label, &hostname, &machine_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn legacy_json_sorts_keys_and_escapes_ascii() {
        assert_eq!(
            legacy_json(&json!({"b": "\u{e9}", "a": [1, 1.0]}), true),
            "{\"a\":[1,1.0],\"b\":\"\\u00e9\"}"
        );
    }
}
