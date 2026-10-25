//! Lossless text for byte strings that are not valid UTF-8 (#362).
//!
//! The legacy helpers decoded Git paths with `surrogateescape`: each undecodable
//! byte `0xXX` became the lone surrogate `U+DCXX`, travelled through `argv`
//! unchanged and was printed by `json.dumps` as `\udcXX`. A Rust `String` cannot
//! hold a lone surrogate, so the same bytes map to the private-use characters
//! `U+F780..=U+F7FF` (`0xF700 + byte`) and only the JSON writer, and the argv
//! encoder, turn them back. A *valid* path that already contains one of those
//! private-use characters would be ambiguous, so [`decode`] refuses it instead
//! of silently changing the bytes (fail closed).
use serde_json::Value;
use std::ffi::OsString;

const BASE: u32 = 0xF700;

fn escaped_byte(ch: char) -> Option<u8> {
    let code = ch as u32;
    (BASE + 0x80..=BASE + 0xFF)
        .contains(&code)
        .then(|| (code - BASE) as u8)
}

/// The stand-in text of a filesystem path (`str(path)` under `surrogateescape`).
pub fn decode_path(path: &std::path::Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        decode(path.as_os_str().as_bytes()).unwrap_or_else(|_| path.to_string_lossy().into_owned())
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned()
    }
}

/// `bytes.decode("utf-8", "surrogateescape")` with the private-use stand-ins.
pub fn decode(bytes: &[u8]) -> Result<String, String> {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                push_valid(&mut out, valid)?;
                return Ok(out);
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                push_valid(&mut out, std::str::from_utf8(valid).unwrap_or(""))?;
                let bad = error.error_len().unwrap_or(after.len());
                for byte in &after[..bad] {
                    out.push(char::from_u32(BASE + u32::from(*byte)).unwrap_or('\u{fffd}'));
                }
                rest = &after[bad..];
            }
        }
    }
}

fn push_valid(out: &mut String, valid: &str) -> Result<(), String> {
    if valid.chars().any(|ch| escaped_byte(ch).is_some()) {
        return Err(
            "a path contains a private-use character reserved for undecodable bytes".into(),
        );
    }
    out.push_str(valid);
    Ok(())
}

/// The filesystem path whose bytes are exactly `bytes` (Python's
/// `Path(bytes.decode("utf-8", "surrogateescape"))` round trip).
pub fn path_from_bytes(bytes: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::path::PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Text for a terminal or log: stand-ins print as the `\udcXX` escapes a
/// `backslashreplace` stream showed for the original surrogates.
pub fn display(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match escaped_byte(ch) {
            Some(byte) => {
                let _ = write!(out, "\\udc{byte:02x}");
            }
            None => out.push(ch),
        }
    }
    out
}

/// The command-line argument whose bytes are exactly the original path bytes.
pub fn arg(text: &str) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = Vec::with_capacity(text.len());
        for ch in text.chars() {
            match escaped_byte(ch) {
                Some(byte) => bytes.push(byte),
                None => {
                    let mut buffer = [0; 4];
                    bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                }
            }
        }
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        OsString::from(text)
    }
}

/// One JSON string as `json.dumps` writes it (ensure_ascii), stand-ins as `\udcXX`.
pub fn json_string(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push('"');
    for ch in text.chars() {
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
                if let Some(byte) = escaped_byte(ch) {
                    let _ = write!(out, "\\udc{byte:02x}");
                } else {
                    for unit in ch.encode_utf16(&mut [0; 2]) {
                        let _ = write!(out, "\\u{unit:04x}");
                    }
                }
            }
        }
    }
    out.push('"');
    out
}

/// `json.dumps(value, separators=(",", ":"))` (ensure_ascii, insertion order)
/// with stand-in characters written as the original `\udcXX` escapes.
pub fn legacy_compact_json(value: &Value) -> String {
    fn string(text: &str, out: &mut String) {
        out.push_str(&json_string(text));
    }
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::String(text) => string(text, out),
            Value::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write(value, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                out.push('{');
                for (index, (key, value)) in map.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    string(key, out);
                    out.push(':');
                    write(value, out);
                }
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invalid_bytes_round_trip_and_print_as_the_legacy_surrogate_escapes() {
        let bytes = b"dir/\xff\xfe-\xe2\x82-caf\xc3\xa9.rs";
        let text = decode(bytes).unwrap();
        assert!(text.ends_with("caf\u{e9}.rs"));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert_eq!(arg(&text).into_vec(), bytes);
        }
        assert_eq!(
            legacy_compact_json(&json!({"path": text})),
            "{\"path\":\"dir/\\udcff\\udcfe-\\udce2\\udc82-caf\\u00e9.rs\"}",
        );
    }

    #[test]
    fn valid_text_is_unchanged_and_ambiguous_private_use_is_refused() {
        assert_eq!(decode("a/b é.rs".as_bytes()).unwrap(), "a/b é.rs");
        assert!(decode("a\u{f7ff}".as_bytes()).is_err());
        assert_eq!(decode("a\u{f700}".as_bytes()).unwrap(), "a\u{f700}");
        assert_eq!(
            legacy_compact_json(&json!({"b": [1, null, true], "a": "x\"\n"})),
            r#"{"b":[1,null,true],"a":"x\"\n"}"#
        );
    }
}
