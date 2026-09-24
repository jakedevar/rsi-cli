//! Skill-integration test for the validator's blocker template.
//!
//! Guards the property: "the blocker handoff itself must validate strict".
//! If this test fails, the resume_handoff skill would emit a blocker that
//! itself fails validation, causing an infinite loop of validator
//! rejections.

use rsi_common::handoff_schema::{ValidationError, ValidationMode, blocker_template, validate};

#[test]
fn empty_errors_blocker_validates_strict() {
    let body = blocker_template("/tmp/nonexistent.md", &[]);
    let v = validate(&body, ValidationMode::Strict);
    assert!(
        v.valid,
        "empty-errors blocker template must validate strict, got: {:?}",
        v.errors
    );
}

#[test]
fn single_error_blocker_validates_strict() {
    let errors = vec![ValidationError {
        field: "Immediate Next Action".to_string(),
        rule: "Presence".to_string(),
        message: "required section `## Immediate Next Action` is missing".to_string(),
    }];
    let body = blocker_template(
        "/home/jake/repo/thoughts/shared/handoffs/general/2026-04-25_broken.md",
        &errors,
    );
    let v = validate(&body, ValidationMode::Strict);
    assert!(
        v.valid,
        "single-error blocker template must validate strict, got: {:?}",
        v.errors
    );
}

#[test]
fn many_errors_blocker_validates_strict() {
    // Big error set — exceeds the 3-error truncation budget on Other Notes.
    let errors: Vec<ValidationError> = (0..20)
        .map(|i| ValidationError {
            field: format!("Field{i}"),
            rule: "Presence".to_string(),
            message: format!(
                "this is a long error message that would blow the 100-word \
                 budget if all 20 errors were emitted (#{i})"
            ),
        })
        .collect();
    let body = blocker_template("/some/path.md", &errors);
    let v = validate(&body, ValidationMode::Strict);
    assert!(
        v.valid,
        "many-errors blocker template must validate strict, got: {:?}",
        v.errors
    );
}

#[test]
fn pathological_path_with_quotes_validates_strict() {
    // Defends against YAML-frontmatter injection via the path field.
    let body = blocker_template("/tmp/has\"quote\nand-newline.md", &[]);
    let v = validate(&body, ValidationMode::Strict);
    assert!(
        v.valid,
        "blocker with quoted/newlined path must still validate strict, got: {:?}",
        v.errors
    );
}

#[test]
fn long_field_name_truncated_in_other_notes() {
    let errors = vec![ValidationError {
        field: "X".repeat(500),
        rule: "Y".repeat(500),
        message: "Z".repeat(500),
    }];
    let body = blocker_template("/p.md", &errors);
    let v = validate(&body, ValidationMode::Strict);
    assert!(
        v.valid,
        "long-field-name blocker must validate strict via truncation, got: {:?}",
        v.errors
    );
}
