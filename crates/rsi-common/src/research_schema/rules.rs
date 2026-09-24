//! Rule-application engine for the v1 research-doc JSON schema.
//!
//! The engine collects ALL violations rather than short-circuiting — the CLI
//! prints every error to stderr so the writer skill can fix them in one round
//! trip.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::error::{Validation, ValidationError, ValidationMode};
use super::schema::ResearchDoc;
use std::collections::HashMap;

/// Current schema version. Bumped only by code change. The validator accepts
/// `1 ≤ version ≤ RESEARCH_SCHEMA_VERSION`. v2 is a strict superset of v1: the
/// only delta is the optional `Finding.id` provenance key, so v1 docs (which
/// omit it) validate green under v2 with zero forced migration.
pub const RESEARCH_SCHEMA_VERSION: u32 = 2;

/// `path:line` or `path:start-end`. Path part allows letters, digits, `.`,
/// `_`, `-`, `/`. The trailing range is `:N` or `:N-M`.
static FILE_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w./-]+:\d+(-\d+)?$").expect("valid regex"));

/// Validate a research-doc JSON in strict mode.
///
/// Thin wrapper that delegates to [`validate_research_json_with_mode`] with
/// [`ValidationMode::Strict`]. Preserved as the public re-export for
/// backwards compatibility with the original RSI-014 API and for callers
/// that don't care about the lenient floor — `probe_or_fallback` is one
/// such caller (the JSON it consumes is always emitted by RSI-014's
/// strict-only writer skill, so a lenient probe would mask drift).
pub fn validate_research_json(content: &str) -> Validation {
    validate_research_json_with_mode(content, ValidationMode::Strict)
}

/// Validate a research-doc JSON. See module-level docs.
///
/// `mode` selects which rules fire:
/// - **Always run** (lenient floor): Step 1 (parse), Step 2 (version pin),
///   Step 3 (`research_question` non-empty), Step 4 (`areas` non-empty).
///   These are the four invariants every research doc — historical or new
///   — must satisfy for a downstream consumer to extract any useful signal.
/// - **Strict only**: Step 5 (`findings` 25-word cap + file-ref regex),
///   Step 6 (`file_refs` range ordering), Step 7 (`open_questions` 25-word
///   cap). Step 5 additionally enforces the v2 provenance-key contract on
///   PRESENT finding ids (`id` non-empty + unique within the doc). These are
///   write-time hygiene rules; legacy docs predate them.
pub fn validate_research_json_with_mode(content: &str, mode: ValidationMode) -> Validation {
    // Step 1: parse into the typed shape. `deny_unknown_fields` catches drift,
    // `serde_json::from_str` catches missing fields and type mismatches.
    let doc: ResearchDoc = match serde_json::from_str(content) {
        Ok(d) => d,
        Err(e) => {
            // Best-effort: peek the version field from the raw JSON for the
            // schema_version report. Fall back to 0 if even that fails.
            let schema_version = serde_json::from_str::<Value>(content)
                .ok()
                .and_then(|v| v.get("version").and_then(Value::as_u64))
                .map(|n| n as u32)
                .unwrap_or(0);
            return Validation {
                valid: false,
                errors: vec![ValidationError {
                    field: "<root>".to_string(),
                    rule: "Parse".to_string(),
                    message: e.to_string(),
                }],
                schema_version,
                mode,
            };
        }
    };

    let mut errors: Vec<ValidationError> = Vec::new();

    // Step 2: version pin. Reject < 1 or > RESEARCH_SCHEMA_VERSION.
    // (Lenient floor — runs in both modes.)
    if doc.version < 1 || doc.version > RESEARCH_SCHEMA_VERSION {
        errors.push(ValidationError {
            field: "version".to_string(),
            rule: "VersionPin".to_string(),
            message: format!(
                "expected 1..={}, got {}",
                RESEARCH_SCHEMA_VERSION, doc.version
            ),
        });
    }

    // Step 3: research_question presence.
    // (Lenient floor — runs in both modes.)
    if doc.research_question.trim().is_empty() {
        errors.push(ValidationError {
            field: "research_question".to_string(),
            rule: "Presence".to_string(),
            message: "research_question must be non-empty".to_string(),
        });
    }

    // Step 4: areas presence (at least one).
    // (Lenient floor — runs in both modes.)
    if doc.areas.is_empty() {
        errors.push(ValidationError {
            field: "areas".to_string(),
            rule: "Presence".to_string(),
            message: "areas must contain at least one entry".to_string(),
        });
    }

    // Step 5: findings rules. (Strict only.)
    if mode.is_strict() {
        // v2 provenance-key hygiene on PRESENT ids only. Missing (`None`) ids
        // stay legal: v1 docs omit the key entirely, so any v1 output keeps its
        // byte-stable shape and remains green under the v2 pin — the strict
        // superset contract. Lenient mode skips this like WordCap/FormatRegex/
        // RangeCheck. Duplicate detection is exact-string scoped; the error
        // message bounds any echoed id so oversized input cannot bloat output.
        let mut first_id_index: HashMap<&str, usize> = HashMap::new();
        for (i, f) in doc.findings.iter().enumerate() {
            if let Some(id) = f.id.as_deref() {
                if id.trim().is_empty() {
                    errors.push(ValidationError {
                        field: format!("findings[{}].id", i),
                        rule: "Presence".to_string(),
                        message: "id must be non-empty when present".to_string(),
                    });
                } else if let Some(&first) = first_id_index.get(id) {
                    errors.push(ValidationError {
                        field: format!("findings[{}].id", i),
                        rule: "UniqueID".to_string(),
                        message: format!(
                            "duplicate finding id {} (first declared at findings[{}])",
                            bounded_repr(id),
                            first
                        ),
                    });
                } else {
                    first_id_index.insert(id, i);
                }
            }
            if count_words(&f.summary) > 25 {
                errors.push(ValidationError {
                    field: format!("findings[{}].summary", i),
                    rule: "WordCap".to_string(),
                    message: format!(
                        "summary exceeds 25-word cap ({} words)",
                        count_words(&f.summary)
                    ),
                });
            }
            if !FILE_REF_RE.is_match(&f.file_ref) {
                errors.push(ValidationError {
                    field: format!("findings[{}].file_ref", i),
                    rule: "FormatRegex".to_string(),
                    message: format!(
                        "expected `path:line` or `path:start-end`, got `{}`",
                        f.file_ref
                    ),
                });
            }
        }
    }

    // Step 6: file_refs rules. `path` presence is a lenient-floor rule (an
    // empty path is structurally broken, not a hygiene-only concern); the
    // RangeCheck on `lines` is strict-only.
    for (i, fr) in doc.file_refs.iter().enumerate() {
        if fr.path.trim().is_empty() {
            errors.push(ValidationError {
                field: format!("file_refs[{}].path", i),
                rule: "Presence".to_string(),
                message: "path must be non-empty".to_string(),
            });
        }
        if mode.is_strict() && fr.lines[0] > fr.lines[1] {
            errors.push(ValidationError {
                field: format!("file_refs[{}].lines", i),
                rule: "RangeCheck".to_string(),
                message: format!(
                    "lines[0] ({}) must be ≤ lines[1] ({})",
                    fr.lines[0], fr.lines[1]
                ),
            });
        }
    }

    // Step 7: open_questions rules. (Strict only.)
    if mode.is_strict() {
        for (i, q) in doc.open_questions.iter().enumerate() {
            if count_words(&q.summary) > 25 {
                errors.push(ValidationError {
                    field: format!("open_questions[{}].summary", i),
                    rule: "WordCap".to_string(),
                    message: format!(
                        "summary exceeds 25-word cap ({} words)",
                        count_words(&q.summary)
                    ),
                });
            }
        }
    }

    Validation {
        valid: errors.is_empty(),
        errors,
        schema_version: doc.version,
        mode,
    }
}

/// Bounded display of an arbitrary provenance id inside an error message.
/// Capped at 32 chars (char-boundary safe) so an oversized id cannot bloat
/// validator output; field paths remain index-based for programmatic access.
fn bounded_repr(id: &str) -> String {
    const CAP: usize = 32;
    let mut chars = id.chars();
    let head: String = chars.by_ref().take(CAP).collect();
    if chars.next().is_some() {
        format!("\"{}\"…", head)
    } else {
        format!("\"{}\"", head)
    }
}

/// Whitespace-split word counter. Acceptable for v1 because input strings come
/// from the master's working set (dispatched-domain names, sub-agent
/// summaries), not arbitrary user paste — there are no zero-width-joiner edge
/// cases. v2 may switch to `unicode-segmentation` if needed.
fn count_words(s: &str) -> usize {
    s.split_whitespace().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_doc() -> String {
        r#"{
            "version": 1,
            "research_question": "How does pipeline detection work?",
            "areas": ["TUI", "Daemon"],
            "findings": [
                {"summary": "PIPELINE_PATH_RE matches md only", "file_ref": "crates/rsid/src/session/types.rs:22", "confidence": "medium"}
            ],
            "file_refs": [
                {"path": "crates/rsid/src/session/types.rs", "lines": [22, 25]}
            ],
            "open_questions": [
                {"summary": "Should v2 add area to findings?", "blocks_planning": false}
            ]
        }"#
        .to_string()
    }

    #[test]
    fn valid_synthetic_doc_passes() {
        let v = validate_research_json(&valid_doc());
        assert!(v.valid, "errors: {:?}", v.errors);
        assert!(v.errors.is_empty());
        assert_eq!(v.schema_version, 1);
    }

    #[test]
    fn version_zero_rejected() {
        let json = valid_doc().replace(r#""version": 1"#, r#""version": 0"#);
        let v = validate_research_json(&json);
        assert!(!v.valid);
        assert_eq!(v.errors.len(), 1);
        assert_eq!(v.errors[0].rule, "VersionPin");
        assert_eq!(v.errors[0].field, "version");
    }

    #[test]
    fn version_two_accepted() {
        // v2 is the current pin (strict superset). A v1-shaped doc labeled
        // version 2 passes VersionPin (and every other rule) unchanged.
        let json = valid_doc().replace(r#""version": 1"#, r#""version": 2"#);
        let v = validate_research_json(&json);
        assert!(v.valid, "errors: {:?}", v.errors);
        assert!(!v.errors.iter().any(|e| e.rule == "VersionPin"));
        assert_eq!(v.schema_version, 2);
    }

    #[test]
    fn version_one_still_accepted() {
        // Backward-compat: v1 docs remain valid under the v2 pin.
        let v = validate_research_json(&valid_doc());
        assert!(v.valid, "errors: {:?}", v.errors);
        assert!(!v.errors.iter().any(|e| e.rule == "VersionPin"));
        assert_eq!(v.schema_version, 1);
    }

    #[test]
    fn version_three_rejected() {
        // One past the current pin: VersionPin rejects it.
        let json = valid_doc().replace(r#""version": 1"#, r#""version": 3"#);
        let v = validate_research_json(&json);
        assert!(!v.valid);
        assert!(v.errors.iter().any(|e| e.rule == "VersionPin"));
    }

    /// PINNING TEST (rules level, the gate): an existing v1 research doc that
    /// carries NO `id` fields validates GREEN through the full strict rule
    /// engine under `RESEARCH_SCHEMA_VERSION = 2`, AND a v2-labeled doc whose
    /// finding carries `id` also validates green (id is inert to the rules).
    #[test]
    fn v1_doc_validates_green_under_v2_pin_and_id_finding_passes() {
        // The v1 fixture (valid_doc) has no `id` anywhere; it must stay GREEN
        // now that the pin is 2.
        assert_eq!(RESEARCH_SCHEMA_VERSION, 2);
        let v1 = validate_research_json(&valid_doc());
        assert!(
            v1.valid,
            "v1 doc must validate under v2 pin: {:?}",
            v1.errors
        );
        assert_eq!(v1.schema_version, 1);

        // A v2 doc whose finding carries an `id` also validates green (strict
        // superset — the provenance key does not trip any rule).
        let v2_with_id = valid_doc()
            .replace(r#""version": 1"#, r#""version": 2"#)
            .replace(
                r#"{"summary": "PIPELINE_PATH_RE matches md only""#,
                r#"{"id": "F-001", "summary": "PIPELINE_PATH_RE matches md only""#,
            );
        let v2 = validate_research_json(&v2_with_id);
        assert!(v2.valid, "v2+id doc must validate: {:?}", v2.errors);
        assert_eq!(v2.schema_version, 2);
    }

    #[test]
    fn empty_areas_rejected() {
        let json = valid_doc().replace(r#""areas": ["TUI", "Daemon"]"#, r#""areas": []"#);
        let v = validate_research_json(&json);
        assert!(!v.valid);
        let presence: Vec<_> = v
            .errors
            .iter()
            .filter(|e| e.rule == "Presence" && e.field == "areas")
            .collect();
        assert_eq!(presence.len(), 1);
    }

    #[test]
    fn empty_research_question_rejected() {
        let json = valid_doc().replace(
            r#""research_question": "How does pipeline detection work?""#,
            r#""research_question": """#,
        );
        let v = validate_research_json(&json);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.rule == "Presence" && e.field == "research_question")
        );
    }

    #[test]
    fn findings_summary_word_cap_violation_detected() {
        let long: String = (0..30).map(|n| format!("word{} ", n)).collect();
        let json = valid_doc().replace("PIPELINE_PATH_RE matches md only", long.trim());
        let v = validate_research_json(&json);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.rule == "WordCap" && e.field == "findings[0].summary")
        );
    }

    #[test]
    fn findings_file_ref_format_violation_detected() {
        let json = valid_doc().replace("crates/rsid/src/session/types.rs:22", "path-without-colon");
        let v = validate_research_json(&json);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.rule == "FormatRegex" && e.field == "findings[0].file_ref")
        );
    }

    #[test]
    fn file_refs_lines_inverted_rejected() {
        let json = valid_doc().replace(r#""lines": [22, 25]"#, r#""lines": [25, 22]"#);
        let v = validate_research_json(&json);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.rule == "RangeCheck" && e.field == "file_refs[0].lines")
        );
    }

    #[test]
    fn open_question_word_cap_violation_detected() {
        let long: String = (0..30).map(|n| format!("word{} ", n)).collect();
        let json = valid_doc().replace("Should v2 add area to findings?", long.trim());
        let v = validate_research_json(&json);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.rule == "WordCap" && e.field == "open_questions[0].summary")
        );
    }

    #[test]
    fn multiple_errors_collected() {
        // 30-word summary + bad file_ref + inverted lines.
        let long: String = (0..30).map(|n| format!("word{} ", n)).collect();
        let json = valid_doc()
            .replace("PIPELINE_PATH_RE matches md only", long.trim())
            .replace("crates/rsid/src/session/types.rs:22", "no-colon-here")
            .replace(r#""lines": [22, 25]"#, r#""lines": [25, 22]"#);
        let v = validate_research_json(&json);
        assert!(!v.valid);
        // At least 3 errors: WordCap on findings[0].summary, FormatRegex on
        // findings[0].file_ref, RangeCheck on file_refs[0].lines.
        assert!(v.errors.len() >= 3, "errors: {:?}", v.errors);
        assert!(v.errors.iter().any(|e| e.rule == "WordCap"));
        assert!(v.errors.iter().any(|e| e.rule == "FormatRegex"));
        assert!(v.errors.iter().any(|e| e.rule == "RangeCheck"));
    }

    #[test]
    fn parse_error_reports_root_field() {
        let v = validate_research_json("{not valid json");
        assert!(!v.valid);
        assert_eq!(v.errors.len(), 1);
        assert_eq!(v.errors[0].field, "<root>");
        assert_eq!(v.errors[0].rule, "Parse");
        assert_eq!(v.schema_version, 0);
    }

    #[test]
    fn strict_mode_records_mode_in_validation() {
        let v = validate_research_json_with_mode(&valid_doc(), ValidationMode::Strict);
        assert!(v.valid, "errors: {:?}", v.errors);
        assert_eq!(v.mode, ValidationMode::Strict);
    }

    #[test]
    fn lenient_mode_records_mode_in_validation() {
        let v = validate_research_json_with_mode(&valid_doc(), ValidationMode::Lenient);
        assert!(v.valid, "errors: {:?}", v.errors);
        assert_eq!(v.mode, ValidationMode::Lenient);
    }

    #[test]
    fn lenient_skips_findings_word_cap() {
        // 30-word summary is a strict-only WordCap violation; lenient passes.
        let long: String = (0..30).map(|n| format!("word{} ", n)).collect();
        let json = valid_doc().replace("PIPELINE_PATH_RE matches md only", long.trim());
        let v = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(v.valid, "lenient should pass; got errors: {:?}", v.errors);
        // Same input under strict still fails.
        let v_strict = validate_research_json_with_mode(&json, ValidationMode::Strict);
        assert!(!v_strict.valid);
    }

    #[test]
    fn lenient_skips_findings_file_ref_regex() {
        let json = valid_doc().replace("crates/rsid/src/session/types.rs:22", "path-without-colon");
        let v = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(v.valid, "lenient should pass; got errors: {:?}", v.errors);
        let v_strict = validate_research_json_with_mode(&json, ValidationMode::Strict);
        assert!(!v_strict.valid);
    }

    #[test]
    fn lenient_skips_file_refs_range_check() {
        let json = valid_doc().replace(r#""lines": [22, 25]"#, r#""lines": [25, 22]"#);
        let v = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(v.valid, "lenient should pass; got errors: {:?}", v.errors);
        let v_strict = validate_research_json_with_mode(&json, ValidationMode::Strict);
        assert!(!v_strict.valid);
    }

    #[test]
    fn lenient_skips_open_question_word_cap() {
        let long: String = (0..30).map(|n| format!("word{} ", n)).collect();
        let json = valid_doc().replace("Should v2 add area to findings?", long.trim());
        let v = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(v.valid, "lenient should pass; got errors: {:?}", v.errors);
        let v_strict = validate_research_json_with_mode(&json, ValidationMode::Strict);
        assert!(!v_strict.valid);
    }

    #[test]
    fn lenient_still_rejects_missing_research_question() {
        // research_question is a lenient-floor rule — empty rejected in BOTH modes.
        let json = valid_doc().replace(
            r#""research_question": "How does pipeline detection work?""#,
            r#""research_question": """#,
        );
        let v = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.rule == "Presence" && e.field == "research_question")
        );
    }

    #[test]
    fn lenient_still_rejects_empty_areas() {
        let json = valid_doc().replace(r#""areas": ["TUI", "Daemon"]"#, r#""areas": []"#);
        let v = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.rule == "Presence" && e.field == "areas")
        );
    }

    #[test]
    fn lenient_still_rejects_version_drift() {
        let json = valid_doc().replace(r#""version": 1"#, r#""version": 99"#);
        let v = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(!v.valid);
        assert!(v.errors.iter().any(|e| e.rule == "VersionPin"));
    }

    #[test]
    fn lenient_still_rejects_parse_error() {
        let v = validate_research_json_with_mode("{not json", ValidationMode::Lenient);
        assert!(!v.valid);
        assert_eq!(v.errors[0].rule, "Parse");
        assert_eq!(v.mode, ValidationMode::Lenient);
    }

    // ---- v2 provenance-key (`Finding.id`) hygiene (RSI #382) ----

    /// Build a v2 research doc whose findings carry exactly the given ids.
    /// Findings are otherwise valid (25-word summaries, legal file_refs) so
    /// provenance-key rules are the only signal under test.
    fn v2_doc_with_ids(ids: &[&str]) -> String {
        let findings: Vec<String> = ids
            .iter()
            .enumerate()
            .map(|(n, id)| {
                format!(
                    r#"{{"id": "{}", "summary": "finding {} note", "file_ref": "src/lib.rs:{}", "confidence": "medium"}}"#,
                    id, n, n + 1
                )
            })
            .collect();
        format!(
            r#"{{
                "version": 2,
                "research_question": "provenance uniqueness probe",
                "areas": ["validation"],
                "findings": [{}],
                "file_refs": [],
                "open_questions": []
            }}"#,
            findings.join(",")
        )
    }

    #[test]
    fn duplicate_finding_ids_rejected_in_strict_v2() {
        let v = validate_research_json(&v2_doc_with_ids(&["F-001", "F-001"]));
        assert!(!v.valid, "errors: {:?}", v.errors);
        let dups: Vec<_> = v
            .errors
            .iter()
            .filter(|e| e.rule == "UniqueID" && e.field == "findings[1].id")
            .collect();
        assert_eq!(dups.len(), 1, "errors: {:?}", v.errors);
        assert!(dups[0].message.contains("F-001"));
        assert!(dups[0].message.contains("findings[0]"));
    }

    #[test]
    fn unique_finding_ids_validate_in_strict_v2() {
        let v = validate_research_json(&v2_doc_with_ids(&["F-001", "F-002"]));
        assert!(v.valid, "errors: {:?}", v.errors);
    }

    #[test]
    fn duplicate_finding_ids_tolerated_in_lenient_v2() {
        let json = v2_doc_with_ids(&["F-001", "F-001"]);
        let lenient = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(
            lenient.valid,
            "lenient must tolerate duplicate ids: {:?}",
            lenient.errors
        );
        let strict = validate_research_json_with_mode(&json, ValidationMode::Strict);
        assert!(!strict.valid);
    }

    #[test]
    fn missing_finding_ids_still_validate_in_strict_v2() {
        // v1 byte-stability under the v2 pin: a v2-labeled doc whose findings
        // omit the provenance key entirely stays green (None id → no rule fires).
        let json = valid_doc().replace(r#""version": 1"#, r#""version": 2"#);
        let v = validate_research_json(&json);
        assert!(v.valid, "errors: {:?}", v.errors);
        assert_eq!(v.schema_version, 2);
    }

    #[test]
    fn empty_and_whitespace_only_finding_id_rejected_in_strict_v2() {
        let v = validate_research_json(&v2_doc_with_ids(&[""]));
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.rule == "Presence" && e.field == "findings[0].id"),
            "errors: {:?}",
            v.errors
        );

        let ws = validate_research_json(&v2_doc_with_ids(&["   "]));
        assert!(
            ws.errors
                .iter()
                .any(|e| e.rule == "Presence" && e.field == "findings[0].id"),
            "whitespace-only id must be rejected: {:?}",
            ws.errors
        );

        // Lenient still tolerates the malformed id (legacy resume-time floor).
        let lenient =
            validate_research_json_with_mode(&v2_doc_with_ids(&[""]), ValidationMode::Lenient);
        assert!(
            lenient.valid,
            "lenient must tolerate empty id: {:?}",
            lenient.errors
        );
    }

    #[test]
    fn duplicate_ids_rejected_in_strict_even_for_v1_labeled_doc() {
        // Explicit decision: the duplicate-present-id rule keys on PRESENCE,
        // not the declared version — a v1-labeled doc carrying duplicate ids is
        // out of contract either way, and genuine v1 docs (no ids) are never
        // touched. Lenient tolerates both, preserving the resume-time floor.
        let json =
            v2_doc_with_ids(&["F-001", "F-001"]).replace(r#""version": 2"#, r#""version": 1"#);
        let v = validate_research_json(&json);
        assert!(!v.valid);
        assert!(
            v.errors.iter().any(|e| e.rule == "UniqueID"),
            "errors: {:?}",
            v.errors
        );
        let lenient = validate_research_json_with_mode(&json, ValidationMode::Lenient);
        assert!(lenient.valid, "lenient errors: {:?}", lenient.errors);
    }

    #[test]
    fn duplicate_id_error_message_is_bounded() {
        // A 64-char id must not be echoed whole into the error message.
        let long = "L".repeat(64);
        let v = validate_research_json(&v2_doc_with_ids(&[&long, &long]));
        assert!(!v.valid);
        let err = v
            .errors
            .iter()
            .find(|e| e.rule == "UniqueID")
            .expect("UniqueID error");
        assert!(
            err.message.len() < 96,
            "message must stay bounded: {:?}",
            err.message
        );
        assert!(
            err.message.contains('…'),
            "expected truncation marker: {:?}",
            err.message
        );
    }

    #[test]
    fn finding_id_uniqueness_is_exact_string_scoped() {
        // Duplicate detection is deliberately conservative: no normalization,
        // no case folding — F-001 / f-001 / F-002 are all distinct ids.
        let v = validate_research_json(&v2_doc_with_ids(&["F-001", "F-002"]));
        assert!(v.valid, "errors: {:?}", v.errors);
        let case = validate_research_json(&v2_doc_with_ids(&["F-001", "f-001"]));
        assert!(
            case.valid,
            "case-distinct ids must validate: {:?}",
            case.errors
        );
    }
}
