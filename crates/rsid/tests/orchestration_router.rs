//! Integration tests for the orchestration router skill loader — the
//! "mouth of the pipeline" frame injected at the start of every leaf-kind
//! session's system prompt.
//!
//! Exercises `preamble::load_orchestration_router()` against the production
//! repo layout (the skill file is committed at
//! `.claude/commands/orchestration_router.md`, one directory shallower than
//! the per-kind preambles).
//!
//! Note: `preamble::harness_root()` uses `OnceLock`, so within this
//! integration-test binary the FIRST call's result wins for the lifetime
//! of the process. Sibling integration-test files (e.g.
//! `preamble_fallback.rs`) get their own `OnceLock` because each integration
//! test file compiles to its own binary.

use rsid::session::preamble;

fn normalized_portable_body(content: &str) -> String {
    let (_, body) = content
        .split_once("\n---\n")
        .expect("portable policy should have YAML frontmatter");
    let mut normalized = body
        .lines()
        .filter(|line| line.trim() != "---")
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    while normalized.contains("\n\n\n") {
        normalized = normalized.replace("\n\n\n", "\n\n");
    }
    normalized.trim().to_owned()
}

#[test]
fn load_orchestration_router_returns_committed_skill_body() {
    let root = preamble::harness_root().expect("harness root");
    let file = std::fs::read_to_string(root.join(".claude/commands/orchestration_router.md"))
        .expect("router file");
    assert_eq!(
        preamble::load_orchestration_router(root).as_deref(),
        Some(file.as_str())
    );
    assert!(file.starts_with("---\ndescription: "));
    assert!(file.contains("\ncapability_class: architect\n"));
}

#[test]
fn router_lives_under_commands_not_shared() {
    // Reasserts the on-disk layout: the router lives at
    // `.claude/commands/orchestration_router.md`, NOT under
    // `.claude/commands/_shared/`. A regression here would mean the
    // loader looks in the wrong place (or the skill was moved into the
    // _shared bucket and silently swept up by per-kind loading).
    let root = preamble::harness_root().expect("harness root should be discoverable");
    let router_path = root.join(".claude/commands/orchestration_router.md");
    assert!(
        router_path.is_file(),
        "router skill must be committed at {:?}",
        router_path
    );
    let shared_router = root.join(".claude/commands/_shared/orchestration_router.md");
    assert!(
        !shared_router.is_file(),
        "router must NOT live under _shared/ — that bucket is for per-kind worker preambles"
    );
}

#[test]
fn portable_orchestration_policy_mirror_stays_in_sync() {
    let root = preamble::harness_root().expect("harness root");
    let skill = std::fs::read_to_string(root.join(".agents/skills/orchestration-router/SKILL.md"))
        .expect("skill mirror");
    let command = std::fs::read_to_string(root.join(".claude/commands/orchestration_router.md"))
        .expect("router command");
    assert_eq!(
        normalized_portable_body(&skill),
        normalized_portable_body(&command)
    );
}

#[test]
fn compact_handoff_examples_match_strict_validator_schema() {
    let stage_contract = "\n## Stage contract\n### Inputs\nStatic input: source\n### Process\nbounded work\n### Outputs\nartifact\n### Verify\nfocused checks passed\n";
    let cases = [
        "PIPELINE HANDOFF — RESEARCH:\nStatus: partial\ndoc_path: /tmp/research.md\nBlocker: one source remains unresolved\nBlocker evidence: symbol absent from current source\n",
        "PIPELINE HANDOFF — PLAN:\nStatus: blocked\ndoc_path: /tmp/plan.md\nBlocker: design cannot proceed safely\nBlocker class: technical_impasse\nBlocker evidence: two required contracts conflict\n",
        "PIPELINE HANDOFF — IMPLEMENTATION:\nStatus: complete\ndoc_path: /tmp/plan.md\nmanifest_path: /tmp/manifest.md\nCommit: 0123456789abcdef\n",
        "PIPELINE HANDOFF — VERIFY:\nStatus: complete\nmanifest_path: /tmp/manifest.md\nDaemon checks: 2/2\n",
    ];

    for handoff in cases {
        let reply = format!("{handoff}{stage_contract}");
        rsi_common::agent_contract::parse_pipeline_handoff_v2(&reply, "")
            .expect("documented compact handoff should satisfy strict V2 schema");
    }
}
