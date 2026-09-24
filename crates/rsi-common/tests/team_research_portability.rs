//! Contract regression for repository-portable team-research discovery.

use std::fs;
use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn canonical_command() -> String {
    fs::read_to_string(repository_root().join(".claude/commands/team_research.md"))
        .expect("read canonical team-research command")
}

fn section<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let start = text.find(start).expect("section start");
    let end = text[start..]
        .find(end)
        .map(|offset| start + offset)
        .expect("section end");
    &text[start..end]
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn team_research_discovers_domains_without_rsi_checkout_assumptions() {
    let command = canonical_command();
    let preamble = section(
        &command,
        "**Worker preamble (binding):**",
        "**Use this over",
    );
    let decomposition = section(&command, "## Step 2: Domain Decomposition", "## Step 3:");
    let relevant = format!("{preamble}\n{decomposition}");

    for forbidden in [
        "/home/jakedevar/rsi",
        "crates/rsi/src",
        "crates/rsid/src",
        "crates/rsi-common",
    ] {
        assert!(
            !relevant.contains(forbidden),
            "portable discovery still assumes {forbidden}"
        );
    }

    let preamble = normalized(preamble);
    assert!(preamble.contains("current repository's project-relative"));
    assert!(preamble.contains("with `role=research`"));
    assert!(preamble.contains("worker contract injected by the active harness"));
    assert!(preamble.contains("Never resolve a worker contract from an absolute checkout"));
    assert!(preamble.contains("or a different repository"));

    let decomposition = normalized(decomposition);
    for required in [
        "bounded discovery pass",
        "Reuse the named files and directories",
        "top-level directories",
        "workspace or package manifests",
        "at most two focused",
        "Do not inventory the whole tree",
        "Derive independent research domains from the discovered ownership boundaries",
        "fewer than three independent domains",
        "route to `/research`",
        "Do not manufacture workers",
        "ordinary subsystem and persistence investigations use `sonnet`",
        "straightforward historical-context or inventory work uses `haiku`",
        "targeted deep-dive into one or two especially complex files uses `opus`",
        "[discovered domain]",
        "[discovered paths/files]",
        "[domain-specific question]",
        "[capability/model]",
    ] {
        assert!(
            decomposition.contains(required),
            "team-research discovery is missing `{required}`"
        );
    }
}
