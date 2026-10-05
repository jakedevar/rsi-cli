// Tests moved out of `rsid-store` (issue #1021 S4); see `mod.rs`.

fn root_of(file: &str) -> std::path::PathBuf {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if file.starts_with("rsid-store/") {
        manifest.join("../rsid-store/src")
    } else {
        manifest.join("src")
    }
}

/// Production (non-test) portion of a source file.
fn production_source(path: &std::path::Path) -> String {
    let source = std::fs::read_to_string(path).unwrap();
    match source.find("\n#[cfg(test)]\nmod tests") {
        Some(index) => source[..index].to_string(),
        None => source,
    }
}

/// Enumerates every `stamp_execution_environment` caller in the daemon so
/// a new provider spawn builder cannot bypass the credential scrub
/// unnoticed: adding a caller must update this list (and its builder
/// test), and the chokepoint itself must call the scrub.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn every_stamp_execution_environment_caller_is_enumerated() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let store_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../rsid-store/src");
    let mut callers = std::collections::BTreeMap::new();
    // Both crates: a new caller in `rsid-store` must be enumerated too (keyed
    // `rsid-store/<path>`).
    for (root, key_prefix) in [(&src, ""), (&store_src, "rsid-store/")] {
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry.unwrap();
            // The vault module and this test file only name the chokepoint.
            if entry.path().extension().is_none_or(|ext| ext != "rs")
                || entry.path().starts_with(root.join("vault"))
                || entry.path().ends_with("store_split_tests/scan.rs")
            {
                continue;
            }
            let source = production_source(entry.path());
            let count = source.matches("stamp_execution_environment(").count()
                - source.matches("fn stamp_execution_environment(").count();
            if count > 0 {
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                callers.insert(format!("{key_prefix}{relative}"), count);
            }
        }
    }
    let expected: std::collections::BTreeMap<String, usize> = [
        // ClaudeClient::launch
        ("claude.rs", 1),
        // CodexClient::build_cmd_with_custom_provider + CodexClient::launch
        ("codex.rs", 2),
        // AgyClient::launch
        ("agy.rs", 1),
        // build_app_server_command
        ("codex_app_server.rs", 1),
    ]
    .into_iter()
    .map(|(file, count)| (file.to_string(), count))
    .collect();
    assert_eq!(callers, expected);

    let claude = production_source(&src.join("claude.rs"));
    let start = claude
        .find("pub(crate) fn stamp_execution_environment(")
        .unwrap();
    let end = start + claude[start..].find("\n}\n").unwrap();
    assert!(
        claude[start..end].contains("crate::vault::scrub_credential_env(cmd);"),
        "the stamp chokepoint must scrub credential env"
    );

    // Spawns outside the chokepoint scrub explicitly.
    for (file, needle) in [
        (
            "agy.rs",
            "crate::vault::scrub_credential_env(&mut command);",
        ),
        (
            "memory/llm.rs",
            "crate::vault::scrub_credential_env(&mut command);",
        ),
        (
            "session/title.rs",
            "crate::vault::scrub_credential_env(&mut command);",
        ),
        (
            "rsid-store/bedrock.rs",
            "crate::vault::scrub_std_credential_env(&mut command);",
        ),
        // #694 K1 rev4 F1: remaining daemon-env-inheriting boundaries.
        // Local bash tool: empty env + SAFE_ENV_VARS allowlist.
        ("openai.rs", ".env_clear();"),
        (
            "integration/guard.rs",
            "crate::vault::scrub_credential_env(&mut command);",
        ),
        (
            "session/harness/tools/git.rs",
            "crate::vault::scrub_credential_env(&mut command);",
        ),
    ] {
        assert!(
            production_source(&root_of(file).join(file.trim_start_matches("rsid-store/")))
                .contains(needle),
            "{file} must scrub credential env explicitly"
        );
    }
    // Credential-free probes: codex --version, catalog and config
    // app-server probes; Bedrock generator, aws region and catalog probes.
    for (file, needle, count) in [
        (
            "codex.rs",
            "crate::vault::scrub_credential_env(&mut command);",
            3,
        ),
        (
            "rsid-store/bedrock.rs",
            "crate::vault::scrub_std_credential_env(&mut command);",
            3,
        ),
    ] {
        assert_eq!(
            production_source(&root_of(file).join(file.trim_start_matches("rsid-store/")))
                .matches(needle)
                .count(),
            count,
            "{file} probe scrub sites"
        );
    }
}
