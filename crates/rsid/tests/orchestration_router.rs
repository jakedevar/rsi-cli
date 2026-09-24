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
    let content = preamble::load_orchestration_router()
        .expect("orchestration_router.md should load from the repo layout");
    // Anchor on behavior-bearing policy. Generated mirrors are checked below.
    assert!(
        content.contains("Always-on routing policy for spawnable RSI sessions"),
        "router body should identify its launch scope"
    );
    assert!(
        content.contains("## Evidence Before Topology"),
        "router body should route from evidence obligations"
    );
    assert!(
        content.contains("**Tier-0**"),
        "router body should declare Tier-0 routing"
    );
    assert!(
        content.contains("**Tier-1**"),
        "router body should declare Tier-1 routing"
    );
    assert!(
        content.contains("**Tier-2**"),
        "router body should declare Tier-2 routing"
    );
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
fn portable_orchestration_policy_bounds_review_and_mirrors_stay_in_sync() {
    let root = preamble::harness_root().expect("harness root should be discoverable");
    let pairs = [
        (
            ".agents/skills/orchestration-router/SKILL.md",
            ".claude/commands/orchestration_router.md",
        ),
        (
            ".agents/skills/master-orchestrate/SKILL.md",
            ".claude/commands/master_orchestrate.md",
        ),
    ];

    for (skill, command) in pairs {
        let skill = std::fs::read_to_string(root.join(skill))
            .expect("portable skill mirror should be readable");
        let command = std::fs::read_to_string(root.join(command))
            .expect("portable command mirror should be readable");
        let skill = normalized_portable_body(&skill);
        let command = normalized_portable_body(&command);

        for required in ["Tier-0", "Tier-1", "Tier-2", "review", "delta re-review"] {
            assert!(
                skill.contains(required),
                "skill missing required policy: {required}"
            );
            assert!(
                command.contains(required),
                "command missing required policy: {required}"
            );
        }

        for removed_cap in [
            "0 children; 0 revisions; 15 minutes",
            "At most 2 children; 1 revision; 90 minutes",
            "At most 6 children; 2 revisions; 1 working day",
        ] {
            assert!(
                !skill.contains(removed_cap),
                "skill retained cap: {removed_cap}"
            );
            assert!(
                !command.contains(removed_cap),
                "command retained cap: {removed_cap}"
            );
        }

        assert_eq!(skill, command, "portable policy mirrors drifted");
    }

    let master = std::fs::read_to_string(root.join(".claude/commands/master_orchestrate.md"))
        .expect("master command should be readable");
    let router = std::fs::read_to_string(root.join(".claude/commands/orchestration_router.md"))
        .expect("router command should be readable");
    let overlay = std::fs::read_to_string(
        root.join(".claude/commands/_shared/master_orchestrate_rsi_overlay.md"),
    )
    .expect("RSI overlay should be readable");
    let program = std::fs::read_to_string(
        root.join(".claude/commands/_shared/master_orchestrate_rsi_program.md"),
    )
    .expect("RSI program reference should be readable");
    let standalone =
        std::fs::read_to_string(root.join(".claude/commands/mwp_incorporate_standalone.md"))
            .expect("standalone compatibility command should be readable");

    for removed_cap in [
        "SOFT_TOKENS",
        "HARD_TOKENS",
        "MAX_GENERATION",
        "generation N+1 of 12",
    ] {
        assert!(
            !master.contains(removed_cap),
            "master retained token/generation cap: {removed_cap}"
        );
    }

    for policy in [&master, &router] {
        assert!(policy.contains("does not reset"));
        assert!(policy.contains("unresolved block"));
        assert!(policy.contains("Review count alone"));
    }
    for exact_budget in [
        "| Tier-0 | 0 rounds |",
        "| Tier-1 | At most 2 rounds",
        "| Tier-2 | 2 rounds:",
        "| Explicit hazardous specialist gate | At most 3 rounds:",
    ] {
        assert!(master.contains(exact_budget), "missing {exact_budget}");
    }
    for exact_budget in [
        "Tier-0: zero",
        "Tier-1: at most two",
        "Tier-2: initial review plus one",
        "three total",
    ] {
        assert!(router.contains(exact_budget), "missing {exact_budget}");
    }
    assert!(master.contains("## Evidence-Obligation Routing"));
    assert!(master.contains("## Combined Investigation And Design"));
    assert!(master.contains("There is no separate documentation worker by default"));
    assert!(master.contains(".claude/commands/_shared/master_orchestrate_rsi_overlay.md"));
    assert!(master.contains("doc_path: <absolute existing"));
    assert!(master.contains("manifest_path: <absolute path"));
    assert!(master.contains("Daemon checks: <passed>/<total"));
    assert!(master.contains("Failed checks: <titles"));
    assert!(master.contains("Blocker: <required when non-complete>"));
    assert!(!master.contains("ROLE: orchestrate-document"));

    assert!(
        master.len() <= 15_000,
        "master entrypoint exceeded prompt budget"
    );
    assert!(
        router.len() <= 5_000,
        "always-on router exceeded prompt budget"
    );
    assert!(
        overlay.len() <= 6_000,
        "base overlay exceeded prompt budget"
    );
    assert!(
        program.len() <= 6_000,
        "program reference exceeded prompt budget"
    );

    assert!(overlay.contains("master_orchestrate_rsi_program.md"));
    assert!(overlay.contains("master_orchestrate_rsi_closure.md"));
    assert!(overlay.contains("Ordinary slice work does not load either reference"));
    assert!(program.contains("## Program Registration And No-Idle"));
    assert!(program.contains("Review Budget In Program Mode"));
    assert!(program.contains("orchestration_outcome_v1:"));
    assert!(program.contains("queue_exhausted"));
    for checkpoint_field in [
        "logical_slice_id",
        "review_rounds_used",
        "review_rounds_remaining",
        "reservation_id",
        "candidate_session_id",
        "state_version",
        "zero in-flight mutating children",
        "sole-active-master ownership",
    ] {
        assert!(
            program.contains(checkpoint_field),
            "program checkpoint missing {checkpoint_field}"
        );
    }

    assert!(standalone.contains("Legacy compatibility fallback"));
    assert!(standalone.contains("Default review budget: Tier-0 zero"));
    assert!(!standalone.contains("After any fix, run review again"));
    assert!(!standalone.contains("ROLE: orchestrate-document"));
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
