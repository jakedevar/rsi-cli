//! Short ids on read verbs (#2312): `AgentGetJob`, `AgentGetStatus`,
//! `AgentGetProgress` and `AgentReadSessionEvents` take a full UUID or a
//! unique prefix of at least [`ID_PREFIX_MIN`] hex characters (the 8-character
//! form notices and ledgers print).
//!
//! This module is the pure half: it classifies an id text and finds the id
//! fields of those four verbs in a params object. The daemon resolves a prefix
//! to the one full id inside the caller's scope
//! (`session::agent_id_prefix`), and the request DTOs keep their typed `Uuid`
//! fields because the daemon rewrites the params before it decodes them.
//! Mutating verbs (`AgentHalt`, `AgentCancelJob`, `AgentSendMessage`, ...)
//! stay strict: a prefix is never an authority target.

use crate::agent_control_schema::AgentControlVerbV1;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

/// The shortest accepted prefix, in hex digits.
pub const ID_PREFIX_MIN: usize = 8;
/// Candidate ids named in an ambiguity refusal.
pub const ID_PREFIX_CANDIDATES_MAX: usize = 10;
/// Refusal when a prefix is shorter than [`ID_PREFIX_MIN`].
pub const ID_PREFIX_TOO_SHORT: &str = "id_prefix_too_short";
/// Refusal when no id inside the caller's scope starts with the prefix. An
/// unknown id and one outside the scope are refused identically.
pub const ID_PREFIX_NOT_FOUND: &str = "id_prefix_not_found";
/// Refusal prefix when more than one id inside the caller's scope matches; the
/// text continues with the matching full ids.
pub const ID_PREFIX_AMBIGUOUS: &str = "id_prefix_ambiguous";
/// JSON-schema `pattern` of an id-or-prefix field.
pub const ID_REF_PATTERN: &str = "^[0-9a-fA-F-]{8,36}$";

/// What kind of record an id field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdRefKind {
    Job,
    Session,
}

/// An id text, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdRef {
    /// A complete UUID (any form `Uuid::parse_str` accepts).
    Full(Uuid),
    /// Lowercase hex digits, hyphens removed, at least [`ID_PREFIX_MIN`].
    Prefix(String),
    /// Hex-looking but shorter than [`ID_PREFIX_MIN`].
    TooShort,
    /// Not an id text at all; left for the strict decoder to refuse.
    Invalid,
}

/// Classify `text` as a full id, an id prefix, or neither.
#[must_use]
pub fn classify_id_ref(text: &str) -> IdRef {
    if let Ok(id) = Uuid::parse_str(text) {
        return IdRef::Full(id);
    }
    if text.is_empty()
        || text.len() > 36
        || !text.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return IdRef::Invalid;
    }
    let hex: String = text
        .chars()
        .filter(|c| *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match hex.len() {
        0 => IdRef::Invalid,
        n if n < ID_PREFIX_MIN => IdRef::TooShort,
        n if n >= 32 => IdRef::Invalid,
        _ => IdRef::Prefix(hex),
    }
}

/// How one verb carries ids: field name, kind, and whether it is an array.
fn id_fields(verb: AgentControlVerbV1) -> &'static [(&'static str, IdRefKind, bool)] {
    match verb {
        AgentControlVerbV1::GetJob => &[("job_id", IdRefKind::Job, false)],
        AgentControlVerbV1::GetStatus | AgentControlVerbV1::ReadSessionEvents => {
            &[("session_id", IdRefKind::Session, false)]
        }
        AgentControlVerbV1::GetProgress => &[("session_ids", IdRefKind::Session, true)],
        _ => &[],
    }
}

/// Whether `verb` accepts id prefixes.
#[must_use]
pub fn verb_accepts_id_prefix(verb: AgentControlVerbV1) -> bool {
    !id_fields(verb).is_empty()
}

fn id_texts<'a>(verb: AgentControlVerbV1, params: &'a Value) -> Vec<(IdRefKind, &'a str)> {
    let mut out = Vec::new();
    for (field, kind, is_array) in id_fields(verb) {
        match params.get(*field) {
            Some(Value::String(text)) if !*is_array => out.push((*kind, text.as_str())),
            Some(Value::Array(items)) if *is_array => {
                out.extend(items.iter().filter_map(|v| v.as_str()).map(|t| (*kind, t)));
            }
            _ => {}
        }
    }
    out
}

/// Every id text of `verb`'s params that is a prefix or too short, in field
/// order, once each. The daemon resolves these.
#[must_use]
pub fn id_prefixes_in(verb: AgentControlVerbV1, params: &Value) -> Vec<(IdRefKind, String)> {
    let mut out: Vec<(IdRefKind, String)> = Vec::new();
    for (kind, text) in id_texts(verb, params) {
        if matches!(classify_id_ref(text), IdRef::Prefix(_) | IdRef::TooShort)
            && !out.iter().any(|(_, seen)| seen == text)
        {
            out.push((kind, text.to_string()));
        }
    }
    out
}

/// Rewrite each prefix text in `params` with its entry in `resolved` (keyed by
/// the original text); a text with no entry is left as it is.
pub fn replace_id_prefixes(
    verb: AgentControlVerbV1,
    params: &mut Value,
    resolved: &HashMap<String, Uuid>,
) {
    let swap = |value: &mut Value| {
        if let Value::String(text) = value
            && let Some(id) = resolved.get(text.as_str())
        {
            *text = id.to_string();
        }
    };
    for (field, _, is_array) in id_fields(verb) {
        match params.get_mut(*field) {
            Some(value @ Value::String(_)) if !*is_array => swap(value),
            Some(Value::Array(items)) if *is_array => items.iter_mut().for_each(swap),
            _ => {}
        }
    }
}

/// `params` with every well-formed prefix replaced by a fixed non-nil UUID, so
/// the strict decoder can check everything else about the request. A text that
/// is too short stays and fails the decoder.
#[must_use]
pub fn with_prefixes_as_full_ids(verb: AgentControlVerbV1, params: &Value) -> Value {
    let mut out = params.clone();
    let placeholder = Uuid::from_u128(1);
    let resolved: HashMap<String, Uuid> = id_texts(verb, params)
        .into_iter()
        .filter(|(_, text)| matches!(classify_id_ref(text), IdRef::Prefix(_)))
        .map(|(_, text)| (text.to_string(), placeholder))
        .collect();
    replace_id_prefixes(verb, &mut out, &resolved);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_full_prefix_short_and_junk() {
        let id = "0e367273-1111-4222-8333-444455556666";
        assert_eq!(
            classify_id_ref(id),
            IdRef::Full(Uuid::parse_str(id).unwrap())
        );
        assert_eq!(
            classify_id_ref("0E367273"),
            IdRef::Prefix("0e367273".into())
        );
        assert_eq!(
            classify_id_ref("0e367273-1111"),
            IdRef::Prefix("0e3672731111".into())
        );
        assert_eq!(classify_id_ref("0e3672"), IdRef::TooShort);
        assert_eq!(classify_id_ref("not-an-id!"), IdRef::Invalid);
        assert_eq!(classify_id_ref(""), IdRef::Invalid);
    }

    #[test]
    fn only_read_verbs_name_id_fields() {
        assert!(verb_accepts_id_prefix(AgentControlVerbV1::GetJob));
        assert!(verb_accepts_id_prefix(AgentControlVerbV1::GetStatus));
        assert!(verb_accepts_id_prefix(AgentControlVerbV1::GetProgress));
        assert!(verb_accepts_id_prefix(
            AgentControlVerbV1::ReadSessionEvents
        ));
        assert!(!verb_accepts_id_prefix(AgentControlVerbV1::Halt));
        assert!(!verb_accepts_id_prefix(AgentControlVerbV1::CancelJob));
        assert!(!verb_accepts_id_prefix(AgentControlVerbV1::SendMessage));
    }

    #[test]
    fn prefixes_are_found_and_replaced_in_arrays_and_scalars() {
        let full = "0e367273-1111-4222-8333-444455556666";
        let params = json!({"session_ids": ["aabbccdd", full, "aabbccdd", "abc"]});
        let found = id_prefixes_in(AgentControlVerbV1::GetProgress, &params);
        assert_eq!(
            found,
            vec![
                (IdRefKind::Session, "aabbccdd".to_string()),
                (IdRefKind::Session, "abc".to_string())
            ]
        );
        let resolved = HashMap::from([("aabbccdd".to_string(), Uuid::from_u128(7))]);
        let mut out = params.clone();
        replace_id_prefixes(AgentControlVerbV1::GetProgress, &mut out, &resolved);
        assert_eq!(out["session_ids"][0], Uuid::from_u128(7).to_string());
        assert_eq!(out["session_ids"][1], full);
        assert_eq!(out["session_ids"][3], "abc");
    }

    #[test]
    fn validation_view_accepts_prefix_but_not_junk() {
        let ok =
            with_prefixes_as_full_ids(AgentControlVerbV1::GetJob, &json!({"job_id": "0e367273"}));
        assert!(AgentControlVerbV1::GetJob.validate_params(&ok).is_ok());
        let short =
            with_prefixes_as_full_ids(AgentControlVerbV1::GetJob, &json!({"job_id": "0e3"}));
        assert!(AgentControlVerbV1::GetJob.validate_params(&short).is_err());
    }
}
