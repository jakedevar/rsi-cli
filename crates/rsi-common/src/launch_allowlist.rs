//! Operator launch-model allowlist (Issue #692).
//!
//! The operator names the model ids a launch may use. The daemon checks the
//! EFFECTIVE model of every launch (request, else project default, else
//! provider default) against the list before any side effect, at the single
//! launch chokepoint, the continuation/rotation preflights and the synchronous
//! `AgentSpawnChild` pre-check, so a lead that ignores its prompt still cannot
//! start a disallowed model. A launch with no determinable model is refused
//! while a list is set. An empty list means unrestricted (the default).
//!
//! This module holds the shared vocabulary: the daemon-config field name, the
//! typed refusal code, the write-time normaliser, and the match predicate. The
//! daemon (`rsid`) and the TUI both depend on it so neither can drift.

/// `UpdateDaemonConfig` / `GetDaemonConfig` field holding the allowlist.
pub const LAUNCH_MODEL_ALLOWLIST_FIELD: &str = "launch_model_allowlist";

/// Typed refusal code carried at the front of every refusal message.
pub const LAUNCH_MODEL_NOT_ALLOWED: &str = "launch_model_not_allowed";

/// Most entries the list may hold.
pub const LAUNCH_MODEL_ALLOWLIST_MAX_ENTRIES: usize = 64;

/// Longest single model id accepted.
pub const LAUNCH_MODEL_ALLOWLIST_MAX_ENTRY_LEN: usize = 200;

/// Normalise a write to the allowlist into its canonical list.
///
/// Accepts `null` (clear), an array of strings, or one string whose entries are
/// separated by commas, semicolons or whitespace (`"clear"`, `"none"`, `"off"`
/// and the empty string clear the list). Entries are trimmed and de-duplicated
/// case-insensitively; the first spelling wins and order is kept.
///
/// # Errors
/// A message when the value has the wrong type, an entry is empty or too long,
/// or the list exceeds [`LAUNCH_MODEL_ALLOWLIST_MAX_ENTRIES`].
pub fn normalize_launch_model_allowlist(value: &serde_json::Value) -> Result<Vec<String>, String> {
    let raw: Vec<String> = match value {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::String(text) => {
            let lowered = text.trim().to_ascii_lowercase();
            if matches!(
                lowered.as_str(),
                "" | "clear" | "none" | "off" | "unrestricted"
            ) {
                Vec::new()
            } else {
                text.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect()
            }
        }
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|text| text.trim().to_string())
                    .ok_or_else(|| "expected an array of model id strings".to_string())
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("expected null, a string, or an array of model id strings".to_string()),
    };
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        if entry.is_empty() {
            return Err("model id entries must not be empty".to_string());
        }
        if entry.chars().count() > LAUNCH_MODEL_ALLOWLIST_MAX_ENTRY_LEN {
            return Err(format!(
                "model id entries are limited to {LAUNCH_MODEL_ALLOWLIST_MAX_ENTRY_LEN} characters"
            ));
        }
        if !out
            .iter()
            .any(|seen| canonical_launch_model(None, seen) == canonical_launch_model(None, &entry))
        {
            out.push(entry);
        }
    }
    if out.len() > LAUNCH_MODEL_ALLOWLIST_MAX_ENTRIES {
        return Err(format!(
            "the allowlist is limited to {LAUNCH_MODEL_ALLOWLIST_MAX_ENTRIES} entries"
        ));
    }
    Ok(out)
}

/// Drops the `[1m]` context-variant tag from a Claude-family id.
///
/// Only the Claude CLI launch path adds and understands the tag (`claude-sonnet-5-5[1m]`
/// is the 1M variant of `claude-sonnet-5-5`, #273/#1038), so it is stripped only for the
/// Claude provider (or a provider-less allowlist entry) and only for a `claude-` id with
/// exactly `[1m]`. Every other bracket suffix is preserved on the wire by the other
/// providers (OpenRouter, the Harness OpenAI-compatible path), so it is a different
/// model string and must not match its bare allowlist entry.
fn strip_claude_one_million_variant(
    provider: Option<crate::types::SessionProvider>,
    model: &str,
) -> &str {
    use crate::types::SessionProvider;
    if !matches!(provider, None | Some(SessionProvider::Claude)) {
        return model;
    }
    let Some(base) = model
        .len()
        .checked_sub(4)
        .filter(|at| model.is_char_boundary(*at))
        .filter(|at| model[*at..].eq_ignore_ascii_case("[1m]"))
        .map(|at| model[..at].trim_end())
    else {
        return model;
    };
    let is_claude = base
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case("claude-"));
    if is_claude { base } else { model }
}

/// The comparison form of a model id: trimmed, lower-cased, without the Claude
/// `[1m]` context-variant tag (see [`strip_claude_one_million_variant`]) and without the routing prefix its provider accepts as
/// an alias (`openrouter/` for OpenRouter, `bedrock/` for Bedrock). With
/// `provider == None` (an allowlist entry, which names no provider) both
/// prefixes are dropped. Comparison after this canonicalisation stays exact.
#[must_use]
pub fn canonical_launch_model(
    provider: Option<crate::types::SessionProvider>,
    model: &str,
) -> String {
    use crate::types::SessionProvider;
    let lowered = strip_claude_one_million_variant(provider, model.trim()).to_ascii_lowercase();
    let prefixes: &[&str] = match provider {
        Some(SessionProvider::OpenRouter) => &["openrouter/"],
        Some(SessionProvider::Bedrock) => &["bedrock/"],
        Some(_) => &[],
        None => &["openrouter/", "bedrock/"],
    };
    for prefix in prefixes {
        if let Some(rest) = lowered.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    lowered
}

/// Whether a launch of `model` on `provider` is allowed under `allowlist`.
///
/// An empty list is unrestricted. A non-empty list admits only a launch whose
/// effective model, canonicalised by [`canonical_launch_model`], equals a
/// canonicalised entry; a launch with no effective model (`None`) cannot be
/// vetted and is refused.
#[must_use]
pub fn launch_model_allowed(
    allowlist: &[String],
    provider: Option<crate::types::SessionProvider>,
    model: Option<&str>,
) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    model.is_some_and(|model| {
        let wanted = canonical_launch_model(provider, model);
        allowlist
            .iter()
            .any(|entry| canonical_launch_model(None, entry) == wanted)
    })
}

/// The refusal message: the typed code, what was requested, the allowed set.
#[must_use]
pub fn launch_model_refusal(allowlist: &[String], model: Option<&str>) -> String {
    let requested = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map_or_else(
            || "no effective model could be determined (none requested, no project or provider default)".to_string(),
            |model| format!("model '{model}'"),
        );
    format!(
        "{LAUNCH_MODEL_NOT_ALLOWED}: {requested} is not on the operator launch-model allowlist; \
         allowed: {}",
        allowlist.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_accepts_string_array_and_clear_spellings() {
        assert_eq!(
            normalize_launch_model_allowlist(&json!("gpt-6-sol, z-ai/glm-5.3-flashx gpt-6-SOL"))
                .unwrap(),
            vec!["gpt-6-sol", "z-ai/glm-5.3-flashx"]
        );
        assert_eq!(
            normalize_launch_model_allowlist(&json!([" claude-opus-5-5 "])).unwrap(),
            vec!["claude-opus-5-5"]
        );
        for clear in [json!(null), json!(""), json!("clear"), json!([])] {
            assert!(
                normalize_launch_model_allowlist(&clear).unwrap().is_empty(),
                "{clear}"
            );
        }
    }

    #[test]
    fn normalize_rejects_bad_shapes() {
        assert!(normalize_launch_model_allowlist(&json!(7)).is_err());
        assert!(normalize_launch_model_allowlist(&json!([1])).is_err());
        assert!(normalize_launch_model_allowlist(&json!([""])).is_err());
        assert!(normalize_launch_model_allowlist(&json!(["x".repeat(201)])).is_err());
        let many: Vec<String> = (0..65).map(|n| format!("m{n}")).collect();
        assert!(normalize_launch_model_allowlist(&json!(many)).is_err());
    }

    #[test]
    fn allowed_is_exact_case_insensitive_and_empty_is_unrestricted() {
        let list = vec![
            "claude-opus-5-5".to_string(),
            "deepseek/deepseek-v4.1-flash".into(),
        ];
        assert!(launch_model_allowed(&[], None, None));
        assert!(launch_model_allowed(&[], None, Some("anything")));
        assert!(launch_model_allowed(&list, None, Some("Claude-Opus-5-5")));
        assert!(!launch_model_allowed(&list, None, Some("claude-sonnet-5")));
        assert!(!launch_model_allowed(
            &list,
            None,
            Some("claude-opus-5-5-extra")
        ));
        assert!(!launch_model_allowed(&list, None, None));
    }

    #[test]
    fn variant_suffix_and_provider_aliases_canonicalise_on_both_sides() {
        use crate::types::SessionProvider::{Bedrock, Claude, Codex, OpenRouter};
        for entry in ["claude-sonnet-5-5", "claude-sonnet-5-5[1m]"] {
            let list = vec![entry.to_string()];
            for model in [
                "claude-sonnet-5-5",
                "claude-sonnet-5-5[1m]",
                "Claude-Sonnet-5-5[1M]",
            ] {
                assert!(
                    launch_model_allowed(&list, Some(Claude), Some(model)),
                    "{entry} vs {model}"
                );
            }
            assert!(!launch_model_allowed(
                &list,
                Some(Claude),
                Some("claude-sonnet-5")
            ));
        }
        let list = vec!["z-ai/glm-5.3-flashx".to_string()];
        assert!(launch_model_allowed(
            &list,
            Some(OpenRouter),
            Some("openrouter/z-ai/glm-5.3-flashx")
        ));
        // A fabricated bracket tag is preserved on the wire by OpenRouter, so it is a
        // different model string and must not ride the bare entry (review of #692).
        for model in [
            "z-ai/glm-5.3-flashx[bogus]",
            "z-ai/glm-5.3-flashx[1m]",
            "openrouter/z-ai/glm-5.3-flashx[bogus]",
        ] {
            assert!(
                !launch_model_allowed(&list, Some(OpenRouter), Some(model)),
                "{model}"
            );
        }
        // Claude ids accept only the `[1m]` variant, not an arbitrary tag.
        let claude = vec!["claude-sonnet-5-5".to_string()];
        for model in ["claude-sonnet-5-5[bogus]", "claude-sonnet-5-5[200k]"] {
            assert!(
                !launch_model_allowed(&claude, Some(Claude), Some(model)),
                "{model}"
            );
        }
        // A non-Claude entry's `[1m]` is part of its name; it does not collapse.
        let tagged = vec!["vendor/model[1m]".to_string()];
        assert!(!launch_model_allowed(
            &tagged,
            Some(OpenRouter),
            Some("vendor/model")
        ));
        assert!(launch_model_allowed(
            &tagged,
            Some(OpenRouter),
            Some("vendor/model[1m]")
        ));
        assert_eq!(
            normalize_launch_model_allowlist(&json!(["vendor/model", "vendor/model[1m]"])).unwrap(),
            vec!["vendor/model", "vendor/model[1m]"]
        );
        // The routing prefix is an alias only for the provider that accepts it.
        assert!(!launch_model_allowed(
            &list,
            Some(Codex),
            Some("openrouter/z-ai/glm-5.3-flashx")
        ));
        let prefixed = vec!["bedrock/vendor.model-v1".to_string()];
        assert!(launch_model_allowed(
            &prefixed,
            Some(Bedrock),
            Some("vendor.model-v1")
        ));
        assert_eq!(
            normalize_launch_model_allowlist(&json!([
                "claude-sonnet-5-5",
                "claude-sonnet-5-5[1m]"
            ]))
            .unwrap(),
            vec!["claude-sonnet-5-5"]
        );
    }

    #[test]
    fn refusal_names_the_allowed_choices() {
        let list = vec!["gpt-6-sol".to_string(), "gpt-6-luna".to_string()];
        let message = launch_model_refusal(&list, Some("claude-sonnet-5"));
        assert!(message.starts_with(LAUNCH_MODEL_NOT_ALLOWED), "{message}");
        assert!(message.contains("claude-sonnet-5"), "{message}");
        assert!(message.contains("gpt-6-sol, gpt-6-luna"), "{message}");
        assert!(launch_model_refusal(&list, None).contains("no effective model"));
    }
}
