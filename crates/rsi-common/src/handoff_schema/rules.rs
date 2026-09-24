//! Handoff v1 schema constant and rule engine.
//!
//! `HANDOFF_V1_SCHEMA` is the single source of truth: a `&'static [Field]`
//! mirroring `<handoff_document>` in `.claude/commands/create_handoff.md`.
//! `validate()` walks the schema, applying each field's rules to the
//! parsed frontmatter and body sections, returning a `Validation` with
//! all errors (not short-circuited — the user wants the full rejection
//! list, not the first failure).
//!
//! Field caps mirror `create_handoff.md:71-134`:
//! - immediate_next_action: 20 words
//! - original_request: 60 words
//! - tasks: ≤5 bullets
//! - critical_references: ≤3 bullets
//! - recent_changes: ≤8 bullets
//! - learnings: ≤8 bullets, ≤150 words total
//! - artifacts: ≤6 bullets
//! - action_items: ≤10 bullets, ≤20 words/bullet
//! - other_notes: ≤3 bullets, ≤100 words total

use crate::handoff_schema::body::{Section, bullet_count, iter_bullets, scan_sections, word_count};
use crate::handoff_schema::error::{Validation, ValidationError, ValidationMode};
use crate::handoff_schema::frontmatter::{
    HandoffFrontmatter, HandoffFrontmatterStrict, split_front_matter,
};
use chrono::Utc;
use std::collections::BTreeMap;

/// Bumped whenever the schema set or rule semantics change. v2 freeze
/// requires a code change to this constant.
pub const HANDOFF_SCHEMA_VERSION: u32 = 1;

/// Whether a field is mandatory in strict mode, lenient mode, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modes {
    Strict,
    Both,
}

impl Modes {
    fn is_required_in(self, mode: ValidationMode) -> bool {
        match (self, mode) {
            (Modes::Both, _) => true,
            (Modes::Strict, ValidationMode::Strict) => true,
            (Modes::Strict, ValidationMode::Lenient) => false,
        }
    }
}

/// Rule kinds applied to a field's value.
#[derive(Debug, Clone)]
pub enum Rule {
    /// Field must exist (section present in body, or frontmatter key
    /// non-None / non-empty).
    Presence,
    /// Body word count ≤ N (whitespace-separated unicode tokens).
    WordCap(usize),
    /// ≤ N top-level bullet items (`^- ` or `^* `).
    ItemCap(usize),
    /// Total body word count ≤ N (used alongside ItemCap, e.g. learnings'
    /// "150 words total" rule).
    TotalWordCap(usize),
    /// Each bullet's body word count ≤ N (e.g. action_items' "20 words/bullet").
    PerBulletWordCap(usize),
}

/// Where this field's value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSource {
    Frontmatter,
    /// `## Heading` section in the body. The variant carries the canonical
    /// heading name (matched case-sensitively).
    Body,
}

#[derive(Debug, Clone)]
pub struct Field {
    /// Canonical name. For `Body` fields this is the `## Heading` text.
    /// For `Frontmatter` fields it's the YAML key (used in error messages).
    pub name: &'static str,
    pub source: FieldSource,
    pub required_in: Modes,
    pub rules: &'static [Rule],
}

/// v1 schema. Order is the order errors will be emitted.
///
/// Field caps come from `.claude/commands/create_handoff.md:71-134`. Lenient
/// mode requires only the four near-universal sections (research §Component
/// E showed these at 94-96% of the live corpus); strict adds two more plus
/// every field cap.
pub static HANDOFF_V1_SCHEMA: &[Field] = &[
    // --- Frontmatter ---
    Field {
        name: "frontmatter.status",
        source: FieldSource::Frontmatter,
        required_in: Modes::Both,
        rules: &[Rule::Presence],
    },
    Field {
        name: "frontmatter.date",
        source: FieldSource::Frontmatter,
        required_in: Modes::Strict,
        rules: &[Rule::Presence],
    },
    Field {
        name: "frontmatter.researcher",
        source: FieldSource::Frontmatter,
        required_in: Modes::Strict,
        rules: &[Rule::Presence],
    },
    Field {
        name: "frontmatter.topic",
        source: FieldSource::Frontmatter,
        required_in: Modes::Strict,
        rules: &[Rule::Presence],
    },
    Field {
        name: "frontmatter.last_updated",
        source: FieldSource::Frontmatter,
        required_in: Modes::Strict,
        rules: &[Rule::Presence],
    },
    Field {
        name: "frontmatter.last_updated_by",
        source: FieldSource::Frontmatter,
        required_in: Modes::Strict,
        rules: &[Rule::Presence],
    },
    // --- Body sections ---
    // Lenient + strict: the four near-universal sections (94-96% corpus).
    Field {
        name: "Task(s)",
        source: FieldSource::Body,
        required_in: Modes::Both,
        rules: &[Rule::Presence, Rule::ItemCap(5)],
    },
    Field {
        name: "Critical References",
        source: FieldSource::Body,
        required_in: Modes::Both,
        rules: &[Rule::Presence, Rule::ItemCap(3)],
    },
    Field {
        name: "Action Items & Next Steps",
        source: FieldSource::Body,
        required_in: Modes::Both,
        rules: &[
            Rule::Presence,
            Rule::ItemCap(10),
            Rule::PerBulletWordCap(20),
        ],
    },
    Field {
        name: "Artifacts",
        source: FieldSource::Body,
        required_in: Modes::Both,
        rules: &[Rule::Presence, Rule::ItemCap(6)],
    },
    // Strict only: present in 42-83% of the corpus.
    Field {
        name: "Immediate Next Action",
        source: FieldSource::Body,
        required_in: Modes::Strict,
        rules: &[Rule::Presence, Rule::WordCap(20)],
    },
    Field {
        name: "Original Request",
        source: FieldSource::Body,
        required_in: Modes::Strict,
        rules: &[Rule::Presence, Rule::WordCap(60)],
    },
    // Strict only: caps on optional-but-defined sections. Presence not
    // required; rules apply iff section exists.
    Field {
        name: "Recent Changes",
        source: FieldSource::Body,
        required_in: Modes::Strict,
        rules: &[Rule::ItemCap(8)],
    },
    Field {
        name: "Learnings",
        source: FieldSource::Body,
        required_in: Modes::Strict,
        rules: &[Rule::ItemCap(8), Rule::TotalWordCap(150)],
    },
    Field {
        name: "Other Notes",
        source: FieldSource::Body,
        required_in: Modes::Strict,
        rules: &[Rule::ItemCap(3), Rule::TotalWordCap(100)],
    },
];

/// Validate a complete handoff document.
pub fn validate(content: &str, mode: ValidationMode) -> Validation {
    let mut errors: Vec<ValidationError> = Vec::new();

    // 1. Split frontmatter; must exist.
    let (yaml_str, body) = match split_front_matter(content) {
        Some(parts) => parts,
        None => {
            errors.push(ValidationError {
                field: "frontmatter".to_string(),
                rule: "Presence".to_string(),
                message: "document missing YAML frontmatter (no `---` fence)".to_string(),
            });
            return Validation {
                valid: false,
                errors,
                schema_version: HANDOFF_SCHEMA_VERSION,
                mode,
            };
        }
    };

    // 2. Deserialize frontmatter. Strict mode rejects unknown keys (drift
    //    catcher); lenient mode tolerates them so legacy documents with
    //    extra keys still validate against the structural contract.
    let fm: Option<HandoffFrontmatter> = if mode.is_strict() {
        match serde_yaml_ng::from_str::<HandoffFrontmatterStrict>(yaml_str) {
            Ok(strict) => Some(HandoffFrontmatter {
                date: strict.date,
                researcher: strict.researcher,
                git_commit: strict.git_commit,
                branch: strict.branch,
                repository: strict.repository,
                topic: strict.topic,
                tags: strict.tags,
                status: strict.status.map(|s| s.as_str().to_string()),
                last_updated: strict.last_updated,
                last_updated_by: strict.last_updated_by,
                doc_type: strict.doc_type,
                schema_version: strict.schema_version,
            }),
            Err(e) => {
                errors.push(ValidationError {
                    field: "frontmatter".to_string(),
                    rule: "Schema".to_string(),
                    message: format!("YAML parse error: {e}"),
                });
                None
            }
        }
    } else {
        match serde_yaml_ng::from_str::<HandoffFrontmatter>(yaml_str) {
            Ok(fm) => Some(fm),
            Err(e) => {
                errors.push(ValidationError {
                    field: "frontmatter".to_string(),
                    rule: "Schema".to_string(),
                    message: format!("YAML parse error: {e}"),
                });
                None
            }
        }
    };

    // 3. Schema-version gate. Absent → v1 (default). Present → must equal
    //    HANDOFF_SCHEMA_VERSION; mismatched versions are a hard reject in
    //    both modes (this is how v2 freeze works).
    if let Some(ref fm) = fm
        && let Some(v) = fm.schema_version
        && v != HANDOFF_SCHEMA_VERSION
    {
        errors.push(ValidationError {
            field: "frontmatter.schema_version".to_string(),
            rule: "VersionMatch".to_string(),
            message: format!(
                "schema_version {v} does not match validator v{HANDOFF_SCHEMA_VERSION}"
            ),
        });
    }

    // 4. Scan body sections.
    let sections = scan_sections(body);

    // 5. Walk schema, apply rules.
    //
    // Lenient mode runs only Presence rules; cardinality (WordCap, ItemCap,
    // TotalWordCap, PerBulletWordCap) applies in strict mode only. This
    // keeps legacy handoffs (pre-April-2026 refactor) compatible at resume
    // time while still preventing drift at write time.
    for field in HANDOFF_V1_SCHEMA {
        let required = field.required_in.is_required_in(mode);
        match field.source {
            FieldSource::Frontmatter => {
                if let Some(ref fm) = fm {
                    apply_frontmatter_rules(field, required, fm, &mut errors);
                } else if required {
                    errors.push(ValidationError {
                        field: field.name.to_string(),
                        rule: "Presence".to_string(),
                        message: "frontmatter unparseable; field cannot be checked".to_string(),
                    });
                }
            }
            FieldSource::Body => {
                apply_body_rules(field, required, mode, &sections, &mut errors);
            }
        }
    }

    Validation {
        valid: errors.is_empty(),
        errors,
        schema_version: HANDOFF_SCHEMA_VERSION,
        mode,
    }
}

fn apply_frontmatter_rules(
    field: &Field,
    required: bool,
    fm: &HandoffFrontmatter,
    errors: &mut Vec<ValidationError>,
) {
    let present = match field.name {
        "frontmatter.status" => fm.status.as_deref().is_some_and(|s| !s.trim().is_empty()),
        "frontmatter.date" => fm.date.as_deref().is_some_and(|s| !s.trim().is_empty()),
        "frontmatter.researcher" => fm
            .researcher
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty()),
        "frontmatter.topic" => fm.topic.as_deref().is_some_and(|s| !s.trim().is_empty()),
        "frontmatter.last_updated" => fm
            .last_updated
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty()),
        "frontmatter.last_updated_by" => fm
            .last_updated_by
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty()),
        _ => false,
    };

    for rule in field.rules {
        match rule {
            Rule::Presence => {
                if required && !present {
                    errors.push(ValidationError {
                        field: field.name.to_string(),
                        rule: "Presence".to_string(),
                        message: "required frontmatter field is missing or empty".to_string(),
                    });
                }
            }
            // Other rule kinds don't apply to frontmatter strings in v1.
            Rule::WordCap(_)
            | Rule::ItemCap(_)
            | Rule::TotalWordCap(_)
            | Rule::PerBulletWordCap(_) => {}
        }
    }
}

fn apply_body_rules(
    field: &Field,
    required: bool,
    mode: ValidationMode,
    sections: &BTreeMap<String, Section>,
    errors: &mut Vec<ValidationError>,
) {
    let section = sections.get(field.name);
    if section.is_none() {
        if required && field.rules.iter().any(|r| matches!(r, Rule::Presence)) {
            errors.push(ValidationError {
                field: field.name.to_string(),
                rule: "Presence".to_string(),
                message: format!("required section `## {}` is missing", field.name),
            });
        }
        return; // No body to apply other rules to.
    }
    let body = &section.unwrap().body_text;

    // Cardinality rules (WordCap, ItemCap, TotalWordCap, PerBulletWordCap)
    // only run in strict mode. Lenient mode is presence-only on the four
    // anchor sections + frontmatter sanity.
    let strict = mode.is_strict();

    for rule in field.rules {
        match rule {
            Rule::Presence => {
                // Already handled above by the section.is_none() branch.
            }
            Rule::WordCap(n) if strict => {
                let wc = word_count(body);
                if wc > *n {
                    errors.push(ValidationError {
                        field: field.name.to_string(),
                        rule: format!("WordCap({n})"),
                        message: format!("{wc} words exceeds cap of {n}"),
                    });
                }
            }
            Rule::ItemCap(n) if strict => {
                let bc = bullet_count(body);
                if bc > *n {
                    errors.push(ValidationError {
                        field: field.name.to_string(),
                        rule: format!("ItemCap({n})"),
                        message: format!("{bc} bullets exceeds cap of {n}"),
                    });
                }
            }
            Rule::TotalWordCap(n) if strict => {
                let wc = word_count(body);
                if wc > *n {
                    errors.push(ValidationError {
                        field: field.name.to_string(),
                        rule: format!("TotalWordCap({n})"),
                        message: format!("{wc} total words exceeds cap of {n}"),
                    });
                }
            }
            Rule::PerBulletWordCap(n) if strict => {
                for (i, bullet) in iter_bullets(body).enumerate() {
                    let wc = word_count(bullet);
                    if wc > *n {
                        errors.push(ValidationError {
                            field: field.name.to_string(),
                            rule: format!("PerBulletWordCap({n})"),
                            message: format!(
                                "bullet #{} has {wc} words exceeding cap of {n}",
                                i + 1
                            ),
                        });
                    }
                }
            }
            // Lenient fallthrough: cardinality rules are skipped.
            Rule::WordCap(_)
            | Rule::ItemCap(_)
            | Rule::TotalWordCap(_)
            | Rule::PerBulletWordCap(_) => {}
        }
    }
}

/// Build a strict-valid `_VALIDATOR_REJECTED.md` blocker handoff body.
///
/// Used by `/resume_handoff` (Step 0) when the validator rejects the
/// requested document. The returned string is a complete handoff document
/// (frontmatter + body) that itself passes strict validation, satisfying
/// the "the blocker handoff itself must validate strict" property tested
/// in `crates/rsi-common/tests/handoff_skill_blocker.rs`.
///
/// Inputs:
/// - `rejected_path` — absolute or repo-relative path of the rejected
///   handoff. Embedded in the topic and `## Critical References`.
/// - `errors` — the validator's `errors[]` array. Truncated and word-
///   capped to fit `## Other Notes` (≤100 words total).
pub fn blocker_template(rejected_path: &str, errors: &[ValidationError]) -> String {
    let now = Utc::now();
    let date = now.to_rfc3339();
    let last_updated = now.format("%Y-%m-%d").to_string();
    let topic = format!(
        "Validator rejected: {}",
        truncate_for_yaml(rejected_path, 80)
    );

    // Compose `## Other Notes` body — keep under 100 total words by
    // emitting at most 3 short lines.
    let mut notes = String::new();
    let mut word_budget: usize = 90;
    for (i, e) in errors.iter().take(3).enumerate() {
        let line = format!(
            "- {}: {} ({})",
            truncate_for_yaml(&e.field, 40),
            truncate_for_yaml(&e.rule, 30),
            truncate_for_yaml(&e.message, 60),
        );
        let line_words = word_count(&line);
        if line_words > word_budget {
            break;
        }
        word_budget -= line_words;
        if i > 0 {
            notes.push('\n');
        }
        notes.push_str(&line);
    }
    if notes.is_empty() {
        notes.push_str("- (no errors captured)");
    }

    format!(
        r#"---
date: "{date}"
researcher: rsi-handoff-validate
git_commit: unknown
branch: unknown
repository: rsi
topic: "{topic}"
tags: [validator, rejected]
status: blocked
last_updated: "{last_updated}"
last_updated_by: rsi-handoff-validate
type: validator_blocker
schema_version: {HANDOFF_SCHEMA_VERSION}
---

## Immediate Next Action

Open the rejected handoff and repair the listed fields then re-run.

## Original Request

Resume from {rejected_path} (rejected by handoff schema validator).

## Task(s)

- Repair handoff schema violations: planned

## Critical References

- {rejected_path}

## Recent Changes

- (validator-generated blocker; no source changes)

## Learnings

- Validator rejection writes a strict-valid blocker handoff for triage.

## Artifacts

- {rejected_path}

## Action Items & Next Steps

- Repair handoff fields listed in Other Notes.
- Re-invoke /resume_handoff against the repaired handoff.

## Other Notes

{notes}
"#
    )
}

/// Truncate a string to N characters with ellipsis, replacing newlines and
/// double-quotes that would break the YAML/markdown frontmatter.
fn truncate_for_yaml(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\r' => ' ',
            '"' => '\'',
            _ => c,
        })
        .collect();
    if cleaned.chars().count() <= max_chars {
        cleaned
    } else {
        let mut out: String = cleaned.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-valid synthetic handoff that should pass strict mode.
    fn good_handoff() -> String {
        r#"---
date: "2026-04-25"
researcher: jake
git_commit: abc123
branch: main
repository: rsi
topic: "Test handoff"
tags: [test]
status: complete
last_updated: "2026-04-25"
last_updated_by: jake
type: implementation_strategy
schema_version: 1
---

## Immediate Next Action

Run cargo build and verify the validator binary exists.

## Original Request

Test handoff with valid frontmatter and all required sections present, used to confirm the strict-mode validator passes a hand-rolled clean fixture.

## Task(s)

- Build validator: done
- Wire skill markdown: done

## Critical References

- crates/rsi-common/src/handoff_schema/rules.rs:1
- .claude/commands/create_handoff.md:71

## Recent Changes

- crates/rsi-common/src/handoff_schema/mod.rs — add module
- crates/rsi-common/Cargo.toml — add binary

## Learnings

- serde deny_unknown_fields catches frontmatter drift cleanly.
- BTreeMap section scanner is simpler than regex sweeps.

## Artifacts

- crates/rsi-common/src/handoff_schema/rules.rs
- crates/rsi-common/src/bin/rsi-handoff-validate.rs

## Action Items & Next Steps

- Push branch and verify cargo build green on CI.
- Manual test against bad handoff fixture.
"#
        .to_string()
    }

    #[test]
    fn valid_synthetic_handoff_passes_strict() {
        let v = validate(&good_handoff(), ValidationMode::Strict);
        assert!(v.valid, "good handoff failed strict: {:?}", v.errors);
        assert!(v.errors.is_empty());
    }

    #[test]
    fn valid_synthetic_handoff_passes_lenient() {
        let v = validate(&good_handoff(), ValidationMode::Lenient);
        assert!(v.valid, "good handoff failed lenient: {:?}", v.errors);
    }

    #[test]
    fn missing_frontmatter_rejected() {
        let doc = "## Just A Heading\n\nbody\n";
        let v = validate(doc, ValidationMode::Lenient);
        assert!(!v.valid);
        assert_eq!(v.errors[0].field, "frontmatter");
    }

    #[test]
    fn strict_rejects_missing_immediate_next_action() {
        let doc = good_handoff().replace(
            "## Immediate Next Action\n\nRun cargo build and verify the validator binary exists.\n\n",
            "",
        );
        let v = validate(&doc, ValidationMode::Strict);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.field == "Immediate Next Action" && e.rule == "Presence"),
            "expected Immediate Next Action Presence failure, got: {:?}",
            v.errors
        );
    }

    #[test]
    fn lenient_accepts_missing_immediate_next_action() {
        let doc = good_handoff().replace(
            "## Immediate Next Action\n\nRun cargo build and verify the validator binary exists.\n\n",
            "",
        );
        let v = validate(&doc, ValidationMode::Lenient);
        assert!(
            v.valid,
            "lenient must tolerate missing INA, got: {:?}",
            v.errors
        );
    }

    #[test]
    fn word_cap_violation_detected() {
        // Replace INA body with 25 words (cap is 20).
        let mut long_body = String::new();
        for i in 0..25 {
            long_body.push_str(&format!("word{i} "));
        }
        let doc = good_handoff().replace(
            "Run cargo build and verify the validator binary exists.",
            long_body.trim(),
        );
        let v = validate(&doc, ValidationMode::Strict);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.field == "Immediate Next Action" && e.rule.starts_with("WordCap")),
            "expected WordCap on INA, got: {:?}",
            v.errors
        );
    }

    #[test]
    fn item_cap_violation_detected() {
        // Replace Critical References with 6 bullets (cap = 3).
        let crefs = "- one\n- two\n- three\n- four\n- five\n- six\n";
        let original = "- crates/rsi-common/src/handoff_schema/rules.rs:1\n- .claude/commands/create_handoff.md:71\n";
        let doc = good_handoff().replace(original, crefs);
        let v = validate(&doc, ValidationMode::Strict);
        assert!(!v.valid);
        assert!(
            v.errors
                .iter()
                .any(|e| e.field == "Critical References" && e.rule.starts_with("ItemCap")),
            "expected ItemCap on Critical References, got: {:?}",
            v.errors
        );
    }

    #[test]
    fn strict_rejects_unknown_frontmatter_key() {
        let doc =
            good_handoff().replace("schema_version: 1", "schema_version: 1\nrogue_field: oops");
        let v = validate(&doc, ValidationMode::Strict);
        assert!(!v.valid);
        assert!(
            v.errors[0].field == "frontmatter" && v.errors[0].rule == "Schema",
            "expected frontmatter Schema error, got: {:?}",
            v.errors
        );
    }

    #[test]
    fn lenient_tolerates_unknown_frontmatter_key() {
        let doc =
            good_handoff().replace("schema_version: 1", "schema_version: 1\nrogue_field: oops");
        let v = validate(&doc, ValidationMode::Lenient);
        assert!(
            v.valid,
            "lenient must tolerate extra keys, got: {:?}",
            v.errors
        );
    }

    #[test]
    fn schema_version_mismatch_rejected() {
        let doc = good_handoff().replace("schema_version: 1", "schema_version: 99");
        let v = validate(&doc, ValidationMode::Lenient);
        assert!(!v.valid);
        assert!(
            v.errors.iter().any(|e| e.rule == "VersionMatch"),
            "expected VersionMatch error, got: {:?}",
            v.errors
        );
    }

    #[test]
    fn lenient_strips_strict_only_fields_from_required_set() {
        // Same input that fails strict on missing INA must pass lenient.
        let mut doc = good_handoff();
        doc = doc.replace(
            "## Immediate Next Action\n\nRun cargo build and verify the validator binary exists.\n\n",
            "",
        );
        doc = doc.replace(
            "## Original Request\n\nTest handoff with valid frontmatter and all required sections present, used to confirm the strict-mode validator passes a hand-rolled clean fixture.\n\n",
            "",
        );
        let v = validate(&doc, ValidationMode::Lenient);
        assert!(
            v.valid,
            "lenient should accept missing INA + Original Request, got: {:?}",
            v.errors
        );
    }
}
