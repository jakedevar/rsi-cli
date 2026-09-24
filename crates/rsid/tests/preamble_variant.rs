//! Integration tests for per-kind preamble loading under the production
//! repo layout. These tests exercise the discovery walk-up from
//! CARGO_MANIFEST_DIR — they assume the variant files committed in
//! Phase 1 are present at `.claude/commands/_shared/`.
//!
//! Note: `preamble::harness_root()` uses `OnceLock`, so within this
//! integration-test binary the FIRST call's result wins for the
//! lifetime of the process. Tests that need to mutate `RSI_HARNESS_ROOT`
//! live in sibling files (e.g. `preamble_fallback.rs`) — each
//! integration-test file is a separate binary, so each gets its own
//! `OnceLock`.

use std::collections::BTreeSet;
use std::fs;

use rsi_common::types::SessionKind;
use rsid::session::preamble;

fn repo_text(relative: &str) -> String {
    let root = preamble::harness_root().expect("repository harness root");
    fs::read_to_string(root.join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn markdown_section<'a>(text: &'a str, heading: &str) -> &'a str {
    let start = text.find(heading).expect("section heading");
    let body_start = start + heading.len();
    let end = text[body_start..]
        .find("\n## ")
        .map_or(text.len(), |offset| body_start + offset);
    &text[start..end]
}

fn evidence_class_tags(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("- `[")?;
            let end = rest.find("]`")?;
            let tag = &rest[..end];
            (!tag.is_empty()
                && tag
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '-'))
            .then(|| tag.to_string())
        })
        .collect()
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn evidence_violations(plan: &str) -> (bool, bool) {
    let mut criteria_level = None;
    let mut inferred_criterion = false;
    for line in plan.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            let level = trimmed
                .chars()
                .take_while(|character| *character == '#')
                .count();
            let heading = trimmed[level..].trim().to_ascii_lowercase();
            if heading.contains("acceptance criteria") || heading.contains("success criteria") {
                criteria_level = Some(level);
            } else if criteria_level.is_some_and(|criteria| level <= criteria) {
                criteria_level = None;
            }
        }
        if criteria_level.is_some() && trimmed.contains("[inferred]") {
            inferred_criterion = true;
        }
    }
    let lower = plan.to_ascii_lowercase();
    let promises_readiness = lower.contains("decision-complete")
        || lower.contains("implementation-ready")
        || lower.contains("ready for implementation")
        || lower.contains("ready to implement");
    let readiness_with_inference = promises_readiness && plan.contains("[inferred]");
    (inferred_criterion, readiness_with_inference)
}

#[test]
fn load_returns_bug_content_when_variant_present() {
    // Discovery walks up from CARGO_MANIFEST_DIR; the real repo has the
    // variant files committed by Phase 1, so this exercises the
    // production path end-to-end.
    let content = preamble::load(SessionKind::Bug).expect("bug variant should load");
    assert!(
        content.to_lowercase().contains("bug"),
        "bug variant content should mention 'bug'; got first 200 chars: {}",
        &content[..content.len().min(200)]
    );
    // Sanity: should be the variant file (frontmatter declares kind: bug),
    // not the base file (frontmatter declares no kind).
    assert!(
        content.contains("kind: bug"),
        "bug variant content should declare kind: bug in frontmatter"
    );
    assert!(
        content.contains("## Evidence classification (HARD)"),
        "bug variant must inherit the base evidence contract"
    );
}

#[test]
fn load_returns_feature_refactor_research_variants() {
    for (kind, expected_token) in [
        (SessionKind::Feature, "kind: feature"),
        (SessionKind::Refactor, "kind: refactor"),
        (SessionKind::Research, "kind: research"),
    ] {
        let content = preamble::load(kind)
            .unwrap_or_else(|| panic!("variant for {:?} should load from disk", kind));
        assert!(
            content.contains(expected_token),
            "variant for {:?} should contain {:?} in frontmatter",
            kind,
            expected_token
        );
        assert!(
            content.contains("## Evidence classification (HARD)"),
            "variant for {kind:?} must inherit the base evidence contract"
        );
    }
}

#[test]
fn load_returns_base_for_kinds_without_variants() {
    // Standard, Task, Story, TaskRabbit have no variant file — should all
    // return identical base content.
    let standard = preamble::load(SessionKind::Standard).expect("base preamble should load");
    let task = preamble::load(SessionKind::Task).expect("base preamble should load");
    let story = preamble::load(SessionKind::Story).expect("base preamble should load");
    let task_rabbit = preamble::load(SessionKind::TaskRabbit).expect("base preamble should load");
    assert_eq!(
        standard, task,
        "Standard and Task should both return base content"
    );
    assert_eq!(
        standard, story,
        "Standard and Story should both return base content"
    );
    assert_eq!(
        standard, task_rabbit,
        "Standard and TaskRabbit should both return base content"
    );
    // Sanity: the base file is the canonical RPI Worker Preamble.
    assert!(
        standard.contains("RPI Worker Preamble"),
        "base content should be the RPI Worker Preamble"
    );
}

#[test]
fn harness_root_discoverable_in_repo_layout() {
    let root = preamble::harness_root();
    assert!(
        root.is_some(),
        "harness root should be discoverable when running tests from the repo layout"
    );
    let root = root.unwrap();
    let base = root.join(".claude/commands/_shared/worker_preamble.md");
    assert!(
        base.is_file(),
        "discovered harness root {:?} should contain base preamble at {:?}",
        root,
        base
    );
}

#[test]
fn evidence_taxonomy_is_shared_and_planning_surfaces_close_inference_loopholes() {
    let base = preamble::load(SessionKind::Task).expect("base preamble");
    let evidence = markdown_section(&base, "## Evidence classification (HARD)");
    assert_eq!(
        evidence_class_tags(evidence),
        BTreeSet::from([
            "inferred".to_string(),
            "observed".to_string(),
            "source".to_string(),
        ]),
        "the base contract must expose exactly the three evidence classes"
    );
    let adversarial_fourth = format!("{evidence}\n- `[verified]` — forbidden fourth class");
    assert_eq!(
        evidence_class_tags(&adversarial_fourth),
        BTreeSet::from([
            "inferred".to_string(),
            "observed".to_string(),
            "source".to_string(),
            "verified".to_string(),
        ]),
        "the extractor must expose a fourth taxonomy bullet"
    );
    let normalized_evidence = collapse_whitespace(evidence);
    for required in [
        "primary artifact or result",
        "Mechanically extract machine-readable facts",
        "current tool surface can express it",
        "forbidden in Acceptance Criteria and Success Criteria",
        "downgrade the plan's readiness",
    ] {
        assert!(
            normalized_evidence.contains(required),
            "missing base rule: {required}"
        );
    }

    let research = preamble::load(SessionKind::Research).expect("research preamble");
    assert_eq!(
        evidence_class_tags(markdown_section(
            &research,
            "## Evidence classification (HARD)"
        )),
        BTreeSet::from([
            "inferred".to_string(),
            "observed".to_string(),
            "source".to_string(),
        ])
    );
    let normalized_research = collapse_whitespace(&research);
    assert!(normalized_research.contains("mechanically extracted from the primary artifact"));
    assert!(normalized_research.contains("current surface is shown to express it"));

    let readiness_rule = collapse_whitespace(
        "`[inferred]` is forbidden in Acceptance Criteria and Success Criteria. A plan \
         with any load-bearing `[inferred]` claim must execute and reclassify the claim \
         or downgrade readiness; it cannot be labeled `decision-complete` or \
         `implementation-ready`, described as `ready for implementation`, or given any \
         equivalent readiness promise.",
    );
    for command in ["plan.md", "team_plan.md", "validate_plan.md"] {
        let text = repo_text(&format!(".claude/commands/{command}"));
        let normalized = collapse_whitespace(&text);
        assert!(
            normalized.contains(&readiness_rule),
            "{command} must carry the identical acceptance/readiness rule"
        );
        for tag in ["[observed]", "[source]", "[inferred]"] {
            assert!(text.contains(tag), "{command} missing {tag}");
        }
        assert!(normalized.contains("primary artifact"));
        assert!(normalized.contains("tool surface"));
    }
}

#[test]
fn inferred_acceptance_fixture_triggers_both_blocking_violations() {
    let fixture = repo_text("crates/rsid/tests/fixtures/inferred_acceptance_plan.md");
    assert_eq!(evidence_violations(&fixture), (true, true));

    let valid_control = r#"---
status: implementation-ready
---
# Cited Control
## Success Criteria
- [ ] [observed] The pinned toolchain was inspected. (`rust-toolchain.toml`)
- [ ] [source] Existing migration fingerprints remain immutable. (`AGENTS.md`)
"#;
    assert_eq!(evidence_violations(valid_control), (false, false));

    let equivalent_readiness = r#"
# Equivalent Readiness Negative
This plan is ready for implementation.

## Implementation Approach
- [inferred] The proposed helper can express the exact schema.
"#;
    assert_eq!(
        evidence_violations(equivalent_readiness),
        (false, true),
        "an equivalent readiness promise must not retain load-bearing inference"
    );
}
