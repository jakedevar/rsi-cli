//! All 12 behavior tests from test_import_rolling_qa_cache.py, plus malformed
//! report/count/fingerprint coverage and byte-level cache serialization.
use super::*;
use tempfile::TempDir;

fn fp(shard: &str) -> Value {
    let inputs = json!({"feature": format!("test-shard-{shard}"), "jobs": 4,
        "rustc": "test rustc", "target_triple": "x86_64-unknown-linux-gnu",
        "host_class": "linux-x86_64@desktop:0123456789abcdef", "test_threads": 4});
    json!({"digest": sha256(legacy_json(&inputs, true)), "inputs": inputs})
}
fn environment(class: &str, jobs: &str) -> Value {
    json!({"spec_env": {"CARGO_BUILD_JOBS": jobs, "CARGO_PROFILE_DEV_DEBUG": "line-tables-only"}, "tmpdir_class": class})
}
fn report(red: bool) -> Value {
    let mut shards: Vec<_> = SHARDS.iter().map(|shard| json!({"name": shard, "run": 1, "passed": 1, "failed": 0, "skipped": 0, "exit_code": 0, "failing_tests": [], "fingerprint": fp(shard), "environment": environment("tmpfs", "4")})).collect();
    if red {
        shards[0]["passed"] = json!(0);
        shards[0]["failed"] = json!(1);
        shards[0]["exit_code"] = json!(100);
        shards[0]["failing_tests"] = json!(["memory::tests::known_red"]);
    }
    json!({"schema_version": 1, "swept_sha": "a".repeat(40), "swept_tree": "b".repeat(40), "complete": true,
        "provenance": {"kind": "qa_sweep", "runner": "laptop", "host": "laptop", "report_ref": "QA.md"},
        "shards": shards, "lanes": LANES.iter().map(|name| json!({"name": name, "exit_code": 0, "failing_tests": []})).collect::<Vec<_>>()})
}
fn import(report: &Value, root: &Path) -> Result<(String, Vec<(String, String)>), ImportError> {
    import_report_with(report, root, &"b".repeat(40), |_, shard, _| Ok(fp(shard)))
}
fn key(shard: &str, class: &str, jobs: &str) -> String {
    let env = environment(class, jobs);
    shard_cache_key(
        &format!("sha256:{}", fp(shard)["digest"].as_str().unwrap()),
        &shard_env_class(env["spec_env"].as_object().unwrap(), class),
    )
    .strip_prefix("sha256:")
    .unwrap()
    .to_owned()
}
fn entry_path(root: &Path, shard: &str, class: &str, jobs: &str) -> std::path::PathBuf {
    root.join("a".repeat(40))
        .join(key(shard, class, jobs))
        .join(format!("{shard}.json"))
}
fn entry(root: &Path, shard: &str) -> Value {
    serde_json::from_slice(&fs::read(entry_path(root, shard, "tmpfs", "4")).unwrap()).unwrap()
}
fn assert_states(rows: &[(String, String)], expected: &str) {
    assert_eq!(rows.len(), 16);
    assert!(rows.iter().all(|(_, state)| state == expected), "{rows:?}");
}
fn assert_empty(root: &Path) {
    assert!(fs::read_dir(root).unwrap().next().is_none());
}

#[test]
fn json_import_reuses_complete_reds_and_records_provenance() {
    let root = TempDir::new().unwrap();
    let report = report(true);
    let (tip, first) = import(&report, root.path()).unwrap();
    assert_eq!(tip, "a".repeat(40));
    assert_states(&first, "imported");
    assert_states(&import(&report, root.path()).unwrap().1, "existing");
    assert_eq!(entry(root.path(), "store-01")["failures"], json!([]));
    assert_eq!(
        entry(root.path(), "store-01")["provenance"],
        "qa:laptop:QA.md"
    );
    assert_eq!(
        entry(root.path(), "memory-01")["failures"],
        json!(["memory::tests::known_red"])
    );
    let text = fs::read_to_string(entry_path(root.path(), "store-01", "tmpfs", "4")).unwrap();
    let expected = format!(
        "{{\"failures\": [], \"fingerprint\": \"sha256:{}\", \"provenance\": \"qa:laptop:QA.md\", \"rolling_sha\": \"{}\", \"schema_version\": 1, \"shard\": \"store-01\"}}",
        key("store-01", "tmpfs", "4"),
        "a".repeat(40)
    );
    assert_eq!(text, expected);
}

#[test]
fn fingerprint_mismatch_refuses_entry() {
    let root = TempDir::new().unwrap();
    let error = import_report_with(
        &report(false),
        root.path(),
        &"b".repeat(40),
        |_, shard, _| {
            let mut local = fp(shard);
            local["digest"] = json!("c".repeat(64));
            Ok(local)
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("fingerprint does not match"));
    assert_empty(root.path());
}

#[test]
fn cross_host_class_refuses_even_with_same_source_and_runner() {
    let root = TempDir::new().unwrap();
    let error = import_report_with(
        &report(false),
        root.path(),
        &"b".repeat(40),
        |_, shard, _| {
            let mut local = fp(shard);
            local["inputs"]["host_class"] = json!("linux-x86_64@cloud:fedcba9876543210");
            Ok(local)
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("host class does not match"));
    assert_empty(root.path());
}

#[test]
fn exact_key_conflict_is_typed_and_preserves_first_result() {
    let root = TempDir::new().unwrap();
    import(&report(false), root.path()).unwrap();
    let error = import(&report(true), root.path()).unwrap_err();
    assert!(matches!(error, ImportError::CacheConflict(_)));
    assert_eq!(entry(root.path(), "memory-01")["failures"], json!([]));
}

#[test]
fn concurrent_writers_leave_complete_matching_entries() {
    let root = TempDir::new().unwrap();
    let report = report(false);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| import(&report, root.path()).unwrap()))
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap().1.len(), 16);
        }
    });
    for shard in SHARDS {
        let entry = entry(root.path(), shard);
        assert_eq!(entry["rolling_sha"], "a".repeat(40));
        assert_eq!(entry["shard"], *shard);
        assert_eq!(
            entry["fingerprint"],
            format!("sha256:{}", key(shard, "tmpfs", "4"))
        );
    }
}

#[test]
fn markdown_machine_block_can_seed_a_qa_entry() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("QA.md");
    fs::write(
        &path,
        format!("# QA result\n\n```qa-sweep-v1\n{}\n```\n", report(false)),
    )
    .unwrap();
    let report = load_report(&path).unwrap();
    import(&report, &root.path().join("cache")).unwrap();
    assert_eq!(
        entry(&root.path().join("cache"), "store-01")["provenance"],
        "qa:laptop:QA.md"
    );
}

#[test]
fn cache_key_mirrors_the_lander_vectors() {
    let spec = json!({"CARGO_BUILD_JOBS": "4", "CARGO_PROFILE_DEV_DEBUG": "line-tables-only", "TMPDIR": "/dev/shm/rsi-landing-gate-x"});
    for (class, env, key) in [
        (
            "tmpfs",
            "sha256:d5da322b638d61ee4a9b2061389d35f57037e881a38e64fb305942e8e63636b9",
            "sha256:5c13edf5b414383bf368da17364ebf409c1d775cce03f5fb9aabc1ce4f25a6ea",
        ),
        (
            "disk",
            "sha256:a68fc171a80ec57da8f351dcf6d5c0edbcdb4f1e682d5508514a59ebdfc645d5",
            "sha256:e5c3dd61019cd7a754751ebca2e9a3f6473c53d1a3aa8a77ee0f9bd90b4c84cc",
        ),
    ] {
        assert_eq!(shard_env_class(spec.as_object().unwrap(), class), env);
        assert_eq!(
            shard_cache_key(&format!("sha256:{}", "a".repeat(64)), env),
            key
        );
    }
}

#[test]
fn matching_environment_import_is_found_where_the_lander_looks() {
    let root = TempDir::new().unwrap();
    let mut report = report(false);
    for row in report["shards"].as_array_mut().unwrap() {
        row["environment"] = environment("disk", "4");
    }
    assert_states(&import(&report, root.path()).unwrap().1, "imported");
    for shard in SHARDS {
        let entry: Value =
            serde_json::from_slice(&fs::read(entry_path(root.path(), shard, "disk", "4")).unwrap())
                .unwrap();
        assert_eq!(
            entry["fingerprint"],
            format!("sha256:{}", key(shard, "disk", "4"))
        );
        assert!(!entry_path(root.path(), shard, "tmpfs", "4").exists());
    }
}

#[test]
fn mismatched_environment_import_is_ignored_by_the_lander() {
    let root = TempDir::new().unwrap();
    let mut report = report(false);
    for row in report["shards"].as_array_mut().unwrap() {
        row["environment"] = environment("tmpfs", "8");
    }
    import(&report, root.path()).unwrap();
    for shard in SHARDS {
        assert!(entry_path(root.path(), shard, "tmpfs", "8").exists());
        assert!(!entry_path(root.path(), shard, "tmpfs", "4").exists());
        assert!(
            !root
                .path()
                .join("a".repeat(40))
                .join(fp(shard)["digest"].as_str().unwrap())
                .exists()
        );
    }
}

#[test]
fn report_without_environment_imports_nothing() {
    let root = TempDir::new().unwrap();
    let mut report = report(false);
    for row in report["shards"].as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("environment");
    }
    assert_states(
        &import(&report, root.path()).unwrap().1,
        "environment_unrecorded",
    );
    assert_empty(root.path());
}

#[test]
fn invalid_environment_is_refused() {
    let root = TempDir::new().unwrap();
    for env in [
        json!({"spec_env": {"A": "1\nTMPDIR=disk"}, "tmpdir_class": "tmpfs"}),
        json!({"spec_env": {"A=B": "1"}, "tmpdir_class": "tmpfs"}),
        json!({"spec_env": {}, "tmpdir_class": "ramfs"}),
        json!({"spec_env": {"CARGO_BUILD_JOBS": 4}, "tmpdir_class": "disk"}),
    ] {
        let mut report = report(false);
        // Validate a late row before any earlier row can be published.
        report["shards"][15]["environment"] = env;
        assert!(
            import(&report, root.path())
                .unwrap_err()
                .to_string()
                .contains("invalid QA environment")
        );
        assert_empty(root.path());
    }
}

#[test]
fn incomplete_report_is_refused() {
    let root = TempDir::new().unwrap();
    let mut report = report(false);
    report["complete"] = json!(false);
    assert!(
        import(&report, root.path())
            .unwrap_err()
            .to_string()
            .contains("complete sweep")
    );
    assert_empty(root.path());
}

#[test]
fn malformed_reports_and_rows_are_refused_before_any_write() {
    let root = TempDir::new().unwrap();
    for mutation in 0..12 {
        let mut report = report(false);
        match mutation {
            0 => report["swept_sha"] = json!("A".repeat(40)),
            1 => report["shards"][15]["name"] = json!("memory-01"),
            2 => report["lanes"][0]["name"] = json!("unknown"),
            3 => report["provenance"]["runner"] = json!(""),
            4 => report["shards"][15]["fingerprint"]["digest"] = json!("c".repeat(64)),
            5 => report["shards"][15]["failing_tests"] = json!(["invalid test name"]),
            6 => report["shards"][15]["failing_tests"] = json!(["a", "a"]),
            7 => report["shards"][15]["failed"] = json!(1),
            8 => report["shards"][15]["passed"] = json!(-1),
            9 => report["shards"][15]["failing_tests"] = json!([123]),
            10 | 11 => {
                let row = &mut report["shards"][15];
                row["fingerprint"]["inputs"]["jobs"] = json!(if mutation == 10 { 0 } else { 65 });
                row["fingerprint"]["digest"] =
                    json!(sha256(legacy_json(&row["fingerprint"]["inputs"], true)));
            }
            _ => unreachable!(),
        }
        assert!(import(&report, root.path()).is_err(), "case {mutation}");
        assert_empty(root.path());
    }
    assert!(
        import_report_with(
            &report(false),
            root.path(),
            &"c".repeat(40),
            |_, shard, _| Ok(fp(shard))
        )
        .unwrap_err()
        .to_string()
        .contains("swept tree")
    );
}

#[test]
fn abnormal_results_are_skipped_and_failure_prefix_is_normalized() {
    let root = TempDir::new().unwrap();
    let mut report = report(true);
    report["shards"][0]["failing_tests"] = json!(["rsid memory::tests::known_red"]);
    report["shards"][15]["exit_code"] = json!(1);
    let rows = import(&report, root.path()).unwrap().1;
    assert_eq!(rows.len(), 16);
    assert_eq!(rows[0], ("store-04".into(), "abnormal_skipped".into()));
    assert_eq!(
        entry(root.path(), "memory-01")["failures"],
        json!(["memory::tests::known_red"])
    );
    assert!(!entry_path(root.path(), "store-04", "tmpfs", "4").exists());
}
