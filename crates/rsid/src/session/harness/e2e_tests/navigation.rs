//! S4 (#2254): a Mercury-shaped session navigates a repository with the native
//! `grep`, `glob`, `list_files` and ranged `read_file` tools, then edits the
//! file it found. The model, the provider and the loop are the real ones; only
//! the model server is scripted, so no test touches the network or a key.

use super::{run_scenario_in, text_reply, tool_call_reply};
use crate::session::harness::tools::HarnessToolRegistry;
use crate::session::harness::tools::file::{EditFileTool, ReadFileTool};
use crate::session::harness::tools::glob::GlobTool;
use crate::session::harness::tools::grep::GrepTool;
use crate::session::harness::tools::list_files::ListFilesTool;
use serde_json::json;
use std::sync::Arc;

const CONFIG_LINES: usize = 60;
const RETRY_LINE: usize = 42;

/// A repo with one source file mentioning `RETRY_LIMIT` on line 42 and an
/// ignored `target/` copy that mentions it too.
fn fixture() -> tempfile::TempDir {
    let dir = crate::test_support::disk_backed_tempdir("session-harness-e2e-navigation-");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let config: String = (1..=CONFIG_LINES)
        .map(|line| {
            if line == RETRY_LINE {
                "const RETRY_LIMIT: u32 = 3;\n".to_string()
            } else {
                format!("// config line {line}\n")
            }
        })
        .collect();
    std::fs::write(root.join("src/config.rs"), config).unwrap();
    std::fs::write(
        root.join("target/debug/stale.rs"),
        "const RETRY_LIMIT: u32 = 99;\n",
    )
    .unwrap();
    dir
}

fn registry() -> HarnessToolRegistry {
    let mut registry = HarnessToolRegistry::new();
    registry.register(Arc::new(GrepTool));
    registry.register(Arc::new(GlobTool));
    registry.register(Arc::new(ListFilesTool));
    registry.register(Arc::new(ReadFileTool));
    registry.register(Arc::new(EditFileTool));
    registry
}

/// The model greps for a constant, reads the neighbouring lines of the one
/// hit, and edits it: the file changes on disk and no `target/` path is ever
/// shown to the model.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn mercury_session_greps_reads_a_range_and_edits_the_file() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply(
                "call_grep",
                "grep",
                &json!({"pattern": "RETRY_LIMIT", "output_mode": "content"}).to_string(),
            ),
            tool_call_reply(
                "call_read",
                "read_file",
                &json!({"path": "src/config.rs", "offset": RETRY_LINE - 2, "limit": 5}).to_string(),
            ),
            tool_call_reply(
                "call_edit",
                "file_edit",
                &json!({
                    "path": "src/config.rs",
                    "old_string": "const RETRY_LIMIT: u32 = 3;",
                    "new_string": "const RETRY_LIMIT: u32 = 5;",
                })
                .to_string(),
            ),
            text_reply("Raised RETRY_LIMIT to 5 in src/config.rs."),
        ],
        "Raise the retry limit to 5.",
        repo.path().to_path_buf(),
    )
    .await;

    // Every navigation tool is advertised to the model.
    assert_eq!(outcome.requests.len(), 4);
    let advertised = outcome.tool_names(0);
    for name in ["grep", "glob", "list_files", "read_file", "file_edit"] {
        assert!(advertised.iter().any(|tool| tool == name), "{advertised:?}");
    }

    // All three calls settle without an error, in order.
    assert_eq!(
        outcome.tool_use_ids(),
        ["call_grep", "call_read", "call_edit"]
    );
    let settled = outcome.settled_calls();
    assert_eq!(settled.len(), 3, "one result per call");
    for (id, is_error, content) in &settled {
        assert!(!is_error, "{id}: {content}");
    }

    // `grep` found the one tracked hit and nothing under the ignored `target/`.
    let grep_message = outcome.tool_message(1, "call_grep");
    assert_eq!(
        grep_message,
        format!("src/config.rs:{RETRY_LINE}:const RETRY_LIMIT: u32 = 3;")
    );

    // `read_file` returned the requested range, numbered, with the footer.
    let read_message = outcome.tool_message(2, "call_read");
    assert!(
        read_message.starts_with(&format!(
            "{}\t// config line {}\n",
            RETRY_LINE - 2,
            RETRY_LINE - 2
        )),
        "{read_message}"
    );
    assert!(read_message.contains(&format!("{RETRY_LINE}\tconst RETRY_LIMIT: u32 = 3;\n")));
    assert!(read_message.ends_with(&format!(
        "[lines {}-{} of {CONFIG_LINES}]\n[more lines: call with offset={}]",
        RETRY_LINE - 2,
        RETRY_LINE + 2,
        RETRY_LINE + 3
    )));

    // The edit changed the file on disk and left the ignored copy alone.
    let edited = std::fs::read_to_string(repo.path().join("src/config.rs")).unwrap();
    assert!(edited.contains("const RETRY_LIMIT: u32 = 5;"), "{edited}");
    assert!(!edited.contains("const RETRY_LIMIT: u32 = 3;"));
    assert_eq!(
        std::fs::read_to_string(repo.path().join("target/debug/stale.rs")).unwrap(),
        "const RETRY_LIMIT: u32 = 99;\n"
    );

    // The session ended with the model's answer, and the key stayed in the header.
    assert!(outcome.events.iter().any(|event| {
        event.event_type == "assistant" && event.data.to_string().contains("Raised RETRY_LIMIT")
    }));
    assert!(
        outcome
            .events
            .iter()
            .any(|event| event.event_type == "result" && event.data["subtype"] == "turn_completed")
    );
    outcome.assert_key_only_in_authorization();
}

/// `glob` and `list_files` give the model the same ignore-aware view that
/// `grep` has: neither shows `target/`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test]
async fn mercury_session_finds_files_with_glob_and_list_files_without_ignored_paths() {
    let repo = fixture();
    let outcome = run_scenario_in(
        registry(),
        vec![
            tool_call_reply(
                "call_glob",
                "glob",
                &json!({"pattern": "**/*.rs"}).to_string(),
            ),
            tool_call_reply(
                "call_list",
                "list_files",
                &json!({"max_depth": 2}).to_string(),
            ),
            text_reply("The source is src/config.rs."),
        ],
        "Where is the config?",
        repo.path().to_path_buf(),
    )
    .await;

    assert_eq!(outcome.tool_use_ids(), ["call_glob", "call_list"]);
    assert_eq!(outcome.tool_message(1, "call_glob"), "src/config.rs");
    let listing = outcome.tool_message(2, "call_list");
    assert_eq!(listing, ".gitignore\nsrc/\nsrc/config.rs");
    outcome.assert_key_only_in_authorization();
}
