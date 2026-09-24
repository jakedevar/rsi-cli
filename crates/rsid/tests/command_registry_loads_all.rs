//! RSI-010 command-frontmatter registry integration test.
//!
//! Walks the real `.claude/commands/*.md` files in the repo and asserts every
//! command file parses cleanly and carries a declared `capability_class`.
//! Provides the canonical "every command has routing intent declared"
//! regression surface — a future command file that forgets to declare a class
//! will fail CI loudly.

use rsid::command_frontmatter::CommandRegistry;
use std::path::{Path, PathBuf};

/// Resolve the repo's `.claude/commands` directory from the crate root.
/// Walks up until a `.claude/commands` directory is found so tests still
/// work from worktrees checked out under `.claude/worktrees/…`.
fn repo_commands_root() -> PathBuf {
    let mut cur = std::env::current_dir().expect("cwd");
    loop {
        let candidate = cur.join(".claude").join("commands");
        if candidate.is_dir() {
            return candidate;
        }
        if !cur.pop() {
            panic!("could not locate `.claude/commands` from cwd");
        }
    }
}

fn tracked_top_level_command_names(root: &Path) -> Option<Vec<String>> {
    let repo_root = root.parent()?.parent()?;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["ls-files", "--", ".claude/commands"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let names = stdout
        .lines()
        .filter_map(|rel_path| {
            let path = repo_root.join(rel_path);
            if path.parent() != Some(root) {
                return None;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                return None;
            }
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .collect();
    Some(names)
}

#[test]
fn registry_loads_all_commands() {
    let root = repo_commands_root();
    let reg = CommandRegistry::load(&root);
    let command_names = tracked_top_level_command_names(&root)
        .expect("git tracked command list should be available in repo checkout");

    assert!(
        reg.len() >= 10,
        "expected ≥10 commands in registry, got {}",
        reg.len()
    );
    assert!(
        !command_names.is_empty(),
        "git found zero tracked command files under {}",
        root.display()
    );

    // Require every tracked top-level command file to be registered and carry
    // routing metadata. Ignored local commands may exist in this directory, but
    // they are user-local runtime inputs, not repository invariants.
    for name in command_names {
        let fm = reg
            .get(&name)
            .unwrap_or_else(|| panic!("command `{}` missing from registry", name));
        assert!(
            fm.capability_class.is_some(),
            "command `{}` must declare a capability_class in frontmatter",
            name
        );
    }
}
