//! Inert recipe coverage regression (#670): inspect embedded source/manifests only.
//! No installer fixture, subprocess, shell, Cargo invocation or filesystem writes.

#[test]
fn rust_isolation_recipe_selects_the_complete_installer_fixture_family() {
    let jobs: toml::Value = toml::from_str(include_str!("../../../.rsi/jobs.toml")).unwrap();
    let recipe = &jobs["recipes"]["install-release-rust-isolation"];
    assert_eq!(recipe["runner"].as_str(), Some("make"));
    assert_eq!(
        recipe["target"].as_str(),
        Some("test-install-release-rust-isolation")
    );

    // Read the active target body, so a suite name in a comment or another
    // target cannot satisfy this check. Keep this focused recipe to one command.
    let mut lines = include_str!("../../../Makefile").lines();
    assert_eq!(
        lines.find(|line| *line == "test-install-release-rust-isolation:"),
        Some("test-install-release-rust-isolation:")
    );
    let commands: Vec<_> = lines
        .take_while(|line| line.starts_with('\t') || line.trim().is_empty())
        .filter_map(|line| line.strip_prefix('\t'))
        .collect();
    assert_eq!(commands.len(), 1, "focused recipe commands: {commands:?}");
    let args: Vec<_> = commands[0].split_whitespace().collect();
    assert_eq!(
        args,
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "rsi-repo-guards",
            "--test",
            "install_release_stage",
            "--test",
            "install_release_links",
            "--test",
            "install_release_provenance",
        ],
        "the focused recipe must select exactly the three isolated installer suites"
    );
}
