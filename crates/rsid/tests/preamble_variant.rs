//! Production layout and loader contract for the base and per-kind preambles.

use rsi_common::types::SessionKind;
use rsid::session::preamble;
use std::fs;

fn root() -> std::path::PathBuf {
    preamble::harness_root()
        .expect("repository harness root")
        .to_path_buf()
}

#[test]
fn variant_loader_composes_base_and_selected_kind() {
    let base = fs::read_to_string(root().join(".claude/commands/_shared/worker_preamble.md"))
        .expect("base preamble");
    for (kind, name) in [
        (SessionKind::Bug, "bug"),
        (SessionKind::Feature, "feature"),
        (SessionKind::Refactor, "refactor"),
        (SessionKind::Research, "research"),
    ] {
        let variant = fs::read_to_string(root().join(format!(
            ".claude/commands/_shared/worker_preamble_{name}.md"
        )))
        .expect("kind preamble");
        assert!(variant.starts_with("---\nversion: "));
        assert!(variant.contains(&format!("\nkind: {name}\n")));
        assert!(variant.contains("\ninherits: worker_preamble.md\n"));
        let loaded = preamble::load(kind, &root()).expect("loaded preamble");
        assert!(loaded.starts_with(&base));
        assert!(loaded.contains(&variant));
    }
}

#[test]
fn kinds_without_variants_load_the_same_base() {
    let standard = preamble::load(SessionKind::Standard, &root()).expect("base preamble");
    for kind in [
        SessionKind::Task,
        SessionKind::Story,
        SessionKind::TaskRabbit,
    ] {
        assert_eq!(
            preamble::load(kind, &root()).expect("base preamble"),
            standard
        );
    }
    assert!(standard.starts_with("---\nversion: "));
    assert!(standard.contains("\nrole_variants: [research, planning, implementation]\n"));
}

#[test]
fn on_demand_references_exist() {
    for path in [
        "docs/agents/routing.md",
        "docs/agents/worker-contract.md",
        "docs/agents/worker-kinds.md",
        "docs/agents/verification.md",
        ".claude/commands/_shared/master_orchestrate_rsi_closure.md",
    ] {
        assert!(root().join(path).is_file(), "missing {path}");
    }
}
