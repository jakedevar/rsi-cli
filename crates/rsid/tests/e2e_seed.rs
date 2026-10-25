//! `rsi-e2e-seed` (local #122): the isolated-state fixture writer is real (it goes through the Store API),
//! confined to a harness-marked temp state directory, and unreachable from the daemon.
//!
//! The tests that run the binary need the `e2e-seed` feature that compiles it; the source scan below runs in
//! every build (the scoped gate builds this target without the feature).

use std::path::Path;

#[cfg(feature = "e2e-seed")]
mod seeder {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    use rsi_common::types::{
        EventType, GraphExecutionUpdate, SessionStatus, WorkflowExecutionStatus,
        WorkflowNodeExecutionState,
    };
    use rsid::store::Store;
    use serde_json::json;
    use uuid::Uuid;

    const SEEDER: &str = env!("CARGO_BIN_EXE_rsi-e2e-seed");
    const MARKER_FILE: &str = ".rsi-e2e-isolated-state";
    const MARKER_TEXT: &str = "rsi-e2e-isolated-state v1";

    fn spec(session_id: Uuid) -> serde_json::Value {
        json!({
            "session": {
                "id": session_id,
                "query": "Fix the answer function",
                "title": "E2E finished session",
                "working_dir": "/tmp",
                "project_id": null,
                "parent_id": null,
            },
            "events": [
                { "event_type": "Message", "role": "User", "content": "Fix the answer function." },
                { "event_type": "ToolUse", "role": "Assistant", "content": "", "tool_name": "Edit",
                  "tool_input": { "file_path": "/tmp/x/src/lib.rs", "old_string": "41", "new_string": "42" },
                  "tool_use_id": "toolu_e2e_1" },
                { "event_type": "Message", "role": "Assistant", "content": "It now returns 42." },
            ],
        })
    }

    fn run_seeder(state_dir: &Path, spec_path: &Path) -> Output {
        Command::new(SEEDER)
            .arg("--state-dir")
            .arg(state_dir)
            .arg("--spec")
            .arg(spec_path)
            .output()
            .expect("run rsi-e2e-seed")
    }

    fn marked_state_dir() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("rsi-e2e-seedtest-")
            .tempdir()
            .expect("temp state dir");
        std::fs::write(dir.path().join(MARKER_FILE), MARKER_TEXT).expect("marker");
        dir
    }

    fn write_spec(dir: &Path, session_id: Uuid) -> PathBuf {
        let path = dir.join("spec.json");
        std::fs::write(&path, spec(session_id).to_string()).expect("spec");
        path
    }

    #[test]
    fn the_seeder_writes_a_finished_session_through_the_store_api() {
        let state = marked_state_dir();
        let id = Uuid::new_v4();
        let spec_path = write_spec(state.path(), id);
        let out = run_seeder(state.path(), &spec_path);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let store = Store::open(&state.path().join("rsi.db")).expect("open seeded store");
        let session = store
            .get_session(id)
            .expect("read")
            .expect("the seeded session exists");
        assert_eq!(session.status, SessionStatus::Completed);
        assert_eq!(session.title.as_deref(), Some("E2E finished session"));
        let events = store.load_events(id).expect("events");
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(events[1].event_type, EventType::ToolUse);
        assert_eq!(events[1].tool_name.as_deref(), Some("Edit"));
        assert_eq!(events[2].content, "It now returns 42.");
    }

    #[test]
    fn a_state_directory_without_the_harness_marker_is_refused_and_untouched() {
        let state = tempfile::Builder::new()
            .prefix("rsi-e2e-seedtest-")
            .tempdir()
            .expect("temp dir");
        let spec_path = write_spec(state.path(), Uuid::new_v4());
        let out = run_seeder(state.path(), &spec_path);
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("marker"));
        assert!(
            !state.path().join("rsi.db").exists(),
            "a refused seed must not create a database"
        );

        // A wrong marker text is as good as none.
        std::fs::write(state.path().join(MARKER_FILE), "something else").expect("marker");
        assert_eq!(run_seeder(state.path(), &spec_path).status.code(), Some(2));
        assert!(!state.path().join("rsi.db").exists());
    }

    #[test]
    fn a_marked_directory_outside_the_system_temp_directory_is_refused() {
        // The checkout sits outside the temp directory on every supported host; skip if one does not.
        let temp = std::env::temp_dir().canonicalize().expect("temp dir");
        let manifest = Path::new(rsi_common::manifest_dir!())
            .canonicalize()
            .expect("manifest dir");
        if manifest.starts_with(&temp)
            || manifest.starts_with("/tmp")
            || manifest.starts_with("/private/tmp")
        {
            return;
        }
        let state = tempfile::Builder::new()
            .prefix("rsi-e2e-outside-")
            .tempdir_in(&manifest)
            .expect("dir in the checkout");
        std::fs::write(state.path().join(MARKER_FILE), MARKER_TEXT).expect("marker");
        let spec_path = write_spec(state.path(), Uuid::new_v4());
        let out = run_seeder(state.path(), &spec_path);
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("temp"));
        assert!(!state.path().join("rsi.db").exists());
    }

    fn session_count(db: &Path) -> i64 {
        rusqlite::Connection::open(db)
            .expect("open scratch db")
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("count sessions")
    }

    /// Hub #3245: a symlinked `rsi.db` must not let the seeder write the database it points at.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_database_is_refused_and_its_target_gets_no_session() {
        let scratch = tempfile::Builder::new()
            .prefix("rsi-e2e-seedtarget-")
            .tempdir()
            .expect("scratch dir");
        let target = scratch.path().join("rsi.db");
        drop(Store::open(&target).expect("create scratch target db"));
        assert_eq!(session_count(&target), 0);

        let state = marked_state_dir();
        std::os::unix::fs::symlink(&target, state.path().join("rsi.db")).expect("symlink rsi.db");
        let id = Uuid::new_v4();
        let spec_path = write_spec(state.path(), id);
        let out = run_seeder(state.path(), &spec_path);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("symlink"));
        assert_eq!(
            session_count(&target),
            0,
            "the symlink target must get no session"
        );
    }

    /// Hub #3245: a symlinked SQLite sidecar is refused before the store is opened.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_wal_sidecar_is_refused() {
        let scratch = tempfile::Builder::new()
            .prefix("rsi-e2e-seedtarget-")
            .tempdir()
            .expect("scratch dir");
        let elsewhere = scratch.path().join("elsewhere-wal");
        std::fs::write(&elsewhere, b"").expect("scratch wal target");

        let state = marked_state_dir();
        std::os::unix::fs::symlink(&elsewhere, state.path().join("rsi.db-wal"))
            .expect("symlink wal");
        let spec_path = write_spec(state.path(), Uuid::new_v4());
        let out = run_seeder(state.path(), &spec_path);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("rsi.db-wal"));
        assert!(
            !state.path().join("rsi.db").exists(),
            "a refused seed must not create a database"
        );
        assert_eq!(std::fs::read(&elsewhere).expect("read wal target"), b"");
    }

    /// The daemon's own regular `rsi.db` (and its sidecars) in a marked directory is still seeded.
    #[test]
    fn a_marked_directory_with_an_existing_plain_database_still_seeds() {
        let state = marked_state_dir();
        let db = state.path().join("rsi.db");
        drop(Store::open(&db).expect("daemon-created db"));
        let id = Uuid::new_v4();
        let spec_path = write_spec(state.path(), id);
        let out = run_seeder(state.path(), &spec_path);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(session_count(&db), 1);
        let store = Store::open(&db).expect("reopen");
        assert!(store.get_session(id).expect("read").is_some());
    }

    #[test]
    fn a_spec_with_no_events_or_unknown_fields_is_refused() {
        let state = marked_state_dir();
        let path = state.path().join("spec.json");
        let mut empty = spec(Uuid::new_v4());
        empty["events"] = json!([]);
        std::fs::write(&path, empty.to_string()).expect("spec");
        assert_eq!(run_seeder(state.path(), &path).status.code(), Some(2));

        let mut extra = spec(Uuid::new_v4());
        extra["session"]["status"] = json!("Running");
        std::fs::write(&path, extra.to_string()).expect("spec");
        assert_eq!(run_seeder(state.path(), &path).status.code(), Some(2));
    }

    /// Local #150: a two-node, session-only workflow (plan -> build) and its finished run.
    fn graph_run(workflow_id: Uuid, execution_id: Uuid) -> serde_json::Value {
        json!({
            "workflow_id": workflow_id,
            "title": "E2E seeded run",
            "project_id": null,
            "execution_id": execution_id,
            "repo_root": "/tmp",
            "base_commit": "0123456789abcdef0123456789abcdef01234567",
            "definition": {
                "version": "1.0", "name": "E2E seeded run", "description": "",
                "nodes": [
                    { "id": "plan", "name": "Plan the fix", "type": "action", "instructions": "Plan the fix." },
                    { "id": "build", "name": "Apply the fix", "type": "action", "instructions": "Apply the fix." },
                ],
                "edges": [{ "source": "plan", "target": "build" }],
                "metadata": { "topology_steps": "{\"plan\":{\"kind\":\"session\"},\"build\":{\"kind\":\"session\"}}" },
            },
            "node_results": { "plan": "plan ready", "build": "built ok" },
        })
    }

    #[test]
    fn the_seeder_writes_a_succeeded_graph_run_with_each_node_result() {
        let state = marked_state_dir();
        let (workflow_id, execution_id) = (Uuid::new_v4(), Uuid::new_v4());
        let mut full = spec(Uuid::new_v4());
        full["graph_run"] = graph_run(workflow_id, execution_id);
        let path = state.path().join("spec.json");
        std::fs::write(&path, full.to_string()).expect("spec");
        let out = run_seeder(state.path(), &path);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let store = Store::open(&state.path().join("rsi.db")).expect("open seeded store");
        let document = store
            .get_workflow_definition(workflow_id)
            .expect("read workflow")
            .expect("the seeded workflow exists");
        assert_eq!(document.workflow.title, "E2E seeded run");
        let (status, finished, workflow): (String, Option<String>, String) = store
            .conn
            .query_row(
                "SELECT status,finished_at,workflow_id FROM topology_executions WHERE id=?1",
                [execution_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("the seeded execution exists");
        assert_eq!(status, "succeeded");
        assert!(finished.is_some(), "a finished run has finished_at");
        assert_eq!(workflow, workflow_id.to_string());

        let mut attempts = store
            .conn
            .prepare("SELECT node_id,status,iteration,attempt_no,output_json FROM topology_node_attempts WHERE execution_id=?1 ORDER BY created_at")
            .expect("prepare");
        let attempts = attempts
            .query_map([execution_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .expect("attempts")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");
        assert_eq!(
            attempts
                .iter()
                .map(|a| (a.0.as_str(), a.1.as_str(), a.2, a.3))
                .collect::<Vec<_>>(),
            vec![("plan", "succeeded", 0, 1), ("build", "succeeded", 0, 1)]
        );
        assert!(attempts[0].4.contains("plan ready"));
        assert!(attempts[1].4.contains("built ok"));

        // The event stream the daemon projects: each node reserved, running and settled succeeded with its
        // result preview, then exactly one `finished` update, the run's `succeeded`.
        let mut events = store
            .conn
            .prepare("SELECT kind,payload_json FROM topology_events WHERE execution_id=?1 ORDER BY execution_seq")
            .expect("prepare");
        let events = events
            .query_map([execution_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("events")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");
        assert_eq!(
            events.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(),
            vec![
                "accepted",
                "running",
                "node_reserved",
                "node_running",
                "node_settled",
                "node_reserved",
                "node_running",
                "node_settled",
                "succeeded",
            ]
        );
        let updates = events
            .iter()
            .map(|(_, payload)| {
                let payload: serde_json::Value = serde_json::from_str(payload).expect("payload");
                serde_json::from_value::<GraphExecutionUpdate>(payload["update"].clone())
                    .expect("a GraphExecutionUpdate")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            updates.iter().map(|u| u.sequence).collect::<Vec<_>>(),
            (1..=9).collect::<Vec<_>>()
        );
        let settled = updates
            .iter()
            .filter(|u| u.node_state == Some(WorkflowNodeExecutionState::Succeeded))
            .map(|u| {
                (
                    u.node_id.clone().unwrap_or_default(),
                    u.output_preview.clone().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(settled.len(), 2);
        assert!(settled[0].0 == "plan" && settled[0].1.contains("plan ready"));
        assert!(settled[1].0 == "build" && settled[1].1.contains("built ok"));
        let last = updates.last().expect("events");
        assert_eq!(last.status, WorkflowExecutionStatus::Succeeded);
        assert!(last.finished);
        assert_eq!(updates.iter().filter(|u| u.finished).count(), 1);
    }

    #[test]
    fn a_graph_run_the_seeder_cannot_record_as_one_clean_pass_is_refused_and_writes_nothing() {
        let cases: [(&str, fn(&mut serde_json::Value)); 4] = [
            ("cycle", |run| {
                run["definition"]["edges"] = json!([{ "source": "plan", "target": "build" }, { "source": "build", "target": "plan" }]);
            }),
            ("not a node", |run| {
                run["node_results"]["ghost"] = json!("x")
            }),
            ("not a session node", |run| {
                run["definition"]["metadata"]["topology_steps"] =
                    json!("{\"plan\":{\"kind\":\"command\"}}");
            }),
            ("40-hex", |run| run["base_commit"] = json!("main")),
        ];
        for (why, break_it) in cases {
            let state = marked_state_dir();
            let mut bad = spec(Uuid::new_v4());
            let mut run = graph_run(Uuid::new_v4(), Uuid::new_v4());
            break_it(&mut run);
            bad["graph_run"] = run;
            let path = state.path().join("spec.json");
            std::fs::write(&path, bad.to_string()).expect("spec");
            let out = run_seeder(state.path(), &path);
            assert_eq!(out.status.code(), Some(2), "{why}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(why),
                "{why}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !state.path().join("rsi.db").exists(),
                "{why}: a refused graph run must not create a database"
            );
        }
    }
}

/// The daemon has no path to this tool: nothing under `src/` (other than the tool itself) names the
/// feature, the binary or its marker, so no RPC, agent verb, flag or setting can fabricate history.
#[test]
fn the_daemon_sources_never_reference_the_seeder() {
    fn walk(dir: &Path, hits: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, hits);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !path.ends_with("src/bin/rsi-e2e-seed.rs")
            {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                for needle in ["e2e-seed", "e2e_seed", "rsi-e2e-isolated-state"] {
                    if text.contains(needle) {
                        hits.push(format!("{}: {needle}", path.display()));
                    }
                }
            }
        }
    }
    let mut hits = Vec::new();
    walk(
        &Path::new(rsi_common::manifest_dir!()).join("src"),
        &mut hits,
    );
    assert!(
        hits.is_empty(),
        "the daemon must not reference the seeder: {hits:?}"
    );
}
