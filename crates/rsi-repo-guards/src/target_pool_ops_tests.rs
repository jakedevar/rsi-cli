//! Pool lifecycle (#453): gc, reclaim-idle, refresh, status, summary, the disk floors
//! and admission. The cases are those of `PoolLifecycle`, `PoolDiskFloor` and the
//! refresh part of `NoPoolReason` of the legacy `test_target_pool.py` (case map in
//! `target_pool_case_map.rs`), on real Git
//! repositories and a stub toolchain; the disk the code sees is stated per test.
use super::*;
use crate::target_pool::{build_key, ready_pools};
use crate::testutil;
use serde_json::json;

struct Fx {
    _stub: tempfile::TempDir,
    _pools: tempfile::TempDir,
    repo: tempfile::TempDir,
    env: Env,
    directory: PathBuf,
    key: String,
    commits: Vec<String>,
}

fn stub_toolchain() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let rustc = dir.path().join("rustc");
    fs::write(&rustc, "#!/bin/sh\nprintf 'rustc 1.0.0\\n'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&rustc, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn commit(dir: &Path, name: &str, text: &str) -> String {
    testutil::write(dir, name, text);
    testutil::git(dir, &["add", name]);
    testutil::git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            name,
        ],
    );
    testutil::git(dir, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

fn fx(extra: &[(&str, &str)]) -> Fx {
    let stub = stub_toolchain();
    let pools = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    testutil::git(repo.path(), &["init", "-q", "-b", "main"]);
    let mut commits = vec![commit(repo.path(), "Cargo.toml", "[workspace]\n")];
    for n in 1..=4 {
        commits.push(commit(repo.path(), &format!("f{n}.txt"), "x\n"));
    }
    let mut env = Env::new();
    env.insert(
        "PATH".into(),
        format!(
            "{}:{}",
            stub.path().display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    env.insert("HOME".into(), std::env::var("HOME").unwrap_or_default());
    env.insert(
        "RSI_TARGET_POOL_ROOT".into(),
        pools.path().to_string_lossy().into_owned(),
    );
    for (name, value) in extra {
        env.insert((*name).into(), (*value).into());
    }
    let git = Git { env: &env };
    let directory = repo_dir(&git, &env, repo.path()).unwrap();
    fs::create_dir_all(&directory).unwrap();
    let key = build_key(repo.path(), &env).unwrap();
    Fx {
        _stub: stub,
        _pools: pools,
        repo,
        env,
        directory,
        key,
        commits,
    }
}

impl Fx {
    fn git(&self) -> Git<'_> {
        Git { env: &self.env }
    }
    fn state(&self, name: &str) -> Option<Value> {
        let text = fs::read_to_string(self.directory.join(format!("{name}.json"))).ok()?;
        serde_json::from_str(&text).ok()
    }
    fn exists(&self, name: &str) -> bool {
        self.directory.join(format!("{name}.json")).exists()
    }
    /// A ready pool of `commits[index]` with `bytes` of artifacts.
    fn pool(&self, name: &str, index: usize, built_at: f64, degraded: bool, bytes: usize) -> Value {
        let worktree = self.directory.join(name);
        let target = worktree.join("target");
        fs::create_dir_all(target.join("debug/deps")).unwrap();
        fs::write(target.join("debug/deps/a.rlib"), vec![b'x'; bytes]).unwrap();
        let mut record = json!({
            "name": name, "sha": self.commits[index], "key": self.key, "state": "ready",
            "target": target, "worktree": worktree, "built_at": built_at,
        });
        if degraded {
            record["degraded"] = json!(true);
        }
        write_marker(&self.directory.join(format!("{name}.json")), &record).unwrap();
        record
    }
    fn gc(&self, options: &GcOptions) -> (Vec<String>, Vec<String>) {
        let mut skipped = vec![];
        let removed = gc(&self.git(), &self.directory, options, &mut skipped);
        (removed, skipped)
    }
}

fn plain(keep: i64) -> GcOptions<'static> {
    GcOptions {
        keep,
        repo: None,
        pressure: false,
        reference: None,
        max_distance: 200,
        filesystem: None,
    }
}

fn names(records: &[Value]) -> Vec<String> {
    records
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

fn ready(name: &str, built_at: f64, degraded: bool) -> Value {
    json!({"name": name, "state": "ready", "built_at": built_at, "degraded": degraded})
}

// ---- gc_plan ----

#[test]
fn gc_keeps_the_newest_n_ready_pools() {
    let records = [
        ready("a", 1.0, false),
        ready("b", 2.0, false),
        ready("c", 3.0, false),
    ];
    let (kept, doomed) = gc_plan(&records, 2, false, &BTreeSet::new());
    assert_eq!(names(&kept), ["c", "b"]);
    assert_eq!(names(&doomed), ["a"]);
}

#[test]
fn gc_pressure_keeps_only_the_newest_pool() {
    let records = [
        ready("a", 1.0, false),
        ready("b", 2.0, false),
        ready("c", 3.0, false),
    ];
    let (kept, doomed) = gc_plan(&records, 5, true, &BTreeSet::new());
    assert_eq!(names(&kept), ["c"]);
    assert_eq!(names(&doomed), ["b", "a"]);
}

#[test]
fn gc_keeps_the_newest_complete_pool_behind_degraded_ones_and_under_pressure_too() {
    let records = [
        ready("old", 1.0, false),
        ready("d1", 2.0, true),
        ready("d2", 3.0, true),
    ];
    let (kept, doomed) = gc_plan(&records, 2, false, &BTreeSet::new());
    assert_eq!(names(&kept), ["d2", "d1", "old"]);
    assert!(doomed.is_empty());
    let (kept, doomed) = gc_plan(&records, 2, true, &BTreeSet::new());
    assert_eq!(names(&kept), ["d2", "old"]);
    assert_eq!(names(&doomed), ["d1"]);
}

#[test]
fn gc_ignores_failed_markers_and_counts_only_ready_pools() {
    let records = [
        ready("a", 1.0, false),
        json!({"name": "f", "state": "failed", "built_at": 9.0}),
        ready("b", 2.0, false),
    ];
    let (kept, doomed) = gc_plan(&records, 1, false, &BTreeSet::new());
    assert_eq!(names(&kept), ["b"]);
    assert_eq!(names(&doomed), ["a"]);
}

#[test]
fn a_stale_pool_is_doomed_even_when_it_is_the_newest_and_the_only_one_and_does_not_use_up_the_keep_count()
 {
    let stale: BTreeSet<String> = ["new".to_string()].into();
    let (kept, doomed) = gc_plan(&[ready("new", 9.0, false)], 2, false, &stale);
    assert!(kept.is_empty());
    assert_eq!(names(&doomed), ["new"]);
    let records = [
        ready("new", 9.0, false),
        ready("a", 2.0, false),
        ready("b", 1.0, false),
    ];
    let (kept, doomed) = gc_plan(&records, 2, false, &stale);
    assert_eq!(names(&kept), ["a", "b"]);
    assert_eq!(names(&doomed), ["new"]);
}

#[test]
fn an_explicit_zero_evicts_every_ready_pool_including_the_last_complete_one() {
    let records = [ready("a", 1.0, false), ready("b", 2.0, true)];
    let (kept, doomed) = gc_plan(&records, 0, false, &BTreeSet::new());
    assert!(kept.is_empty());
    assert_eq!(names(&doomed), ["b", "a"]);
}

#[test]
fn the_keep_count_comes_from_the_operator_setting() {
    let mut env = Env::new();
    assert_eq!(default_keep(&env), 2);
    env.insert("RSI_TARGET_POOL_KEEP".into(), "5".into());
    assert_eq!(default_keep(&env), 5);
    env.insert("RSI_TARGET_POOL_KEEP".into(), "0".into());
    assert_eq!(default_keep(&env), 1);
    env.insert("RSI_TARGET_POOL_KEEP".into(), "many".into());
    assert_eq!(default_keep(&env), 2);
}

// ---- gc on disk ----

#[test]
fn gc_removes_what_the_plan_dooms_and_keeps_the_marker_and_log_rules() {
    let fx = fx(&[]);
    for (i, built) in [1.0, 2.0, 3.0].iter().enumerate() {
        fx.pool(&format!("p{i}-{}", fx.key), i, *built, false, 10);
    }
    fs::write(fx.directory.join(format!("p0-{}.log", fx.key)), "log").unwrap();
    let (removed, skipped) = fx.gc(&plain(2));
    assert_eq!(removed, [format!("p0-{}", fx.key)]);
    assert!(skipped.is_empty());
    assert!(!fx.exists(&format!("p0-{}", fx.key)));
    assert!(!fx.directory.join(format!("p0-{}", fx.key)).exists());
    assert!(!fx.directory.join(format!("p0-{}.log", fx.key)).exists());
    assert!(fx.exists(&format!("p1-{}", fx.key)) && fx.exists(&format!("p2-{}", fx.key)));
}

#[test]
fn gc_never_removes_a_pool_a_seed_is_reading_and_names_it() {
    let fx = fx(&[]);
    let name = format!("p0-{}", fx.key);
    fx.pool(&name, 0, 1.0, false, 10);
    fx.pool(&format!("p1-{}", fx.key), 1, 2.0, false, 10);
    let reader = use_lock(&fx.directory, &name, false).expect("shared lock");
    let (removed, skipped) = fx.gc(&plain(1));
    assert!(removed.is_empty());
    assert_eq!(skipped, [name.clone()]);
    assert!(fx.exists(&name));
    drop(reader);
    let (removed, _) = fx.gc(&plain(1));
    assert_eq!(removed, [name]);
}

#[test]
fn gc_zero_evicts_the_last_complete_pool_only_after_its_reader_finishes() {
    let fx = fx(&[]);
    let name = format!("only-{}", fx.key);
    fx.pool(&name, 0, 1.0, false, 10);
    let reader = use_lock(&fx.directory, &name, false).unwrap();
    let (removed, skipped) = fx.gc(&plain(0));
    assert!(removed.is_empty());
    assert_eq!(skipped, [name.clone()]);
    drop(reader);
    assert_eq!(fx.gc(&plain(0)).0, [name]);
}

#[test]
fn a_pool_gc_is_removing_is_not_offered_to_a_seed() {
    let fx = fx(&[]);
    let name = format!("p0-{}", fx.key);
    fx.pool(&name, 0, 1.0, false, 10);
    let _gc = use_lock(&fx.directory, &name, true).expect("exclusive lock");
    let claimed = crate::target_pool::claim_pool(
        &fx.directory,
        &ready_pools(&fx.directory, &fx.key),
        |left| left.first().cloned(),
    );
    assert!(claimed.is_none());
}

#[test]
fn a_stale_gc_cannot_delete_a_pool_rebuilt_since_its_plan() {
    let fx = fx(&[]);
    let name = format!("p0-{}", fx.key);
    let planned = fx.pool(&name, 0, 1.0, false, 10);
    assert!(same_generation(&fx.directory, &planned));
    // The pool is removed and rebuilt under the same name (a later built_at).
    fx.pool(&name, 0, 5.0, false, 10);
    assert!(!same_generation(&fx.directory, &planned));
    fs::remove_file(fx.directory.join(format!("{name}.json"))).unwrap();
    assert!(!same_generation(&fx.directory, &planned));
}

#[test]
fn gc_collects_pools_beyond_the_seed_distance_of_the_tip_and_keeps_near_or_unknown_ones() {
    let fx = fx(&[]);
    let repo = fx.repo.path();
    testutil::git(repo, &["branch", "-f", "rolling", "HEAD"]);
    let behind = format!("behind-{}", fx.key);
    let near = format!("near-{}", fx.key);
    fx.pool(&behind, 0, 9.0, false, 10); // 4 commits behind, the newest and listed first
    fx.pool(&near, 3, 8.0, false, 10); // 1 commit behind
    let unknown = json!({
        "name": "ghost", "sha": "0000000000000000000000000000000000000000", "key": fx.key, "state": "ready",
        "target": fx.directory.join("ghost/target"), "worktree": "/nonexistent", "built_at": 7.0,
    });
    write_marker(&fx.directory.join("ghost.json"), &unknown).unwrap();
    let records: Vec<Value> = [&behind, &near, &"ghost".to_string()]
        .iter()
        .map(|n| fx.state(n).unwrap())
        .collect();
    let git = fx.git();
    let stale = stale_pool_names(&git, &records, "rolling", 2, Some(repo));
    assert_eq!(stale, BTreeSet::from([behind.clone()]));
    let (removed, _) = fx.gc(&GcOptions {
        reference: Some("rolling"),
        max_distance: 2,
        repo: Some(repo),
        ..plain(5)
    });
    assert_eq!(removed, [behind]);
    assert!(fx.exists(&near) && fx.exists("ghost"));
    // An emptied ref keeps stale pools.
    let again = fx.pool(&format!("again-{}", fx.key), 0, 6.0, false, 10);
    let (removed, _) = fx.gc(&GcOptions {
        reference: None,
        max_distance: 2,
        repo: Some(repo),
        ..plain(5)
    });
    assert!(removed.is_empty());
    assert!(fx.exists(again["name"].as_str().unwrap()));
}

#[test]
fn an_imported_pool_is_aged_through_the_checkout_its_directory_belongs_to() {
    let fx = fx(&[]);
    testutil::git(fx.repo.path(), &["branch", "-f", "rolling", "HEAD"]);
    let imported = json!({
        "name": "imp", "sha": fx.commits[1], "state": "ready", "built_at": 1.0,
        "worktree": fx.directory.join("imp"), "target": fx.directory.join("imp/target"),
    });
    let git = fx.git();
    assert_eq!(pool_age_commits(&git, &imported, "rolling", None), None);
    assert_eq!(
        pool_age_commits(&git, &imported, "rolling", Some(fx.repo.path())),
        Some(3)
    );
    assert_eq!(
        pool_age_commits(&git, &imported, "no-such-ref", Some(fx.repo.path())),
        Some(3),
        "falls back to rolling"
    );
}

#[test]
fn the_gc_command_reports_removed_and_in_use_pools_as_json_and_as_lines() {
    let fx = fx(&[]);
    let a = format!("a-{}", fx.key);
    let b = format!("b-{}", fx.key);
    fx.pool(&a, 0, 1.0, false, 10);
    fx.pool(&b, 1, 2.0, false, 10);
    let mut out = vec![];
    let code = gc_command(
        &GcCommand {
            repo: Some(fx.repo.path()),
            keep: Some(1),
            pressure: false,
            json: true,
            reference: None,
            max_distance: 200,
        },
        &fx.env,
        &mut out,
    )
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("{{\"removed\": [\"{a}\"], \"skipped_in_use\": []}}\n")
    );
    let reader = use_lock(&fx.directory, &b, false).unwrap();
    let mut out = vec![];
    gc_command(
        &GcCommand {
            repo: Some(fx.repo.path()),
            keep: Some(0),
            pressure: false,
            json: false,
            reference: None,
            max_distance: 200,
        },
        &fx.env,
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("skipped {b}: in use\n")
    );
    drop(reader);
}

#[test]
fn gc_with_a_repo_records_the_checkout_of_an_imported_pool_directory() {
    let fx = fx(&[]);
    assert!(known_repo(&fx.directory).is_none());
    let mut out = vec![];
    gc_command(
        &GcCommand {
            repo: Some(fx.repo.path()),
            keep: Some(2),
            pressure: false,
            json: true,
            reference: None,
            max_distance: 200,
        },
        &fx.env,
        &mut out,
    )
    .unwrap();
    assert_eq!(
        known_repo(&fx.directory).map(|p| fs::canonicalize(p).unwrap()),
        Some(fs::canonicalize(fx.repo.path()).unwrap())
    );
}

// ---- summary and status ----

#[test]
fn summary_names_the_newest_good_pool_its_size_and_a_newer_failure() {
    let fx = fx(&[]);
    testutil::git(fx.repo.path(), &["branch", "-f", "rolling", "HEAD"]);
    let good = format!("good-{}", fx.key);
    fx.pool(&good, 2, 5.0, false, 123);
    fx.pool(&format!("old-{}", fx.key), 1, 1.0, false, 7);
    write_marker(
        &fx.directory.join("bad.json"),
        &json!({"name": "bad", "sha": fx.commits[4], "state": "failed", "built_at": 8.0,
                "reason": ABORT_FLOOR, "detail": "stopped"}),
    )
    .unwrap();
    record_event(
        &fx.directory,
        "skip",
        fields(json!({"reason": SKIP_HEADROOM, "detail": "later"})),
    );
    let git = fx.git();
    let summary = summarise(&git, std::slice::from_ref(&fx.directory), "rolling", None);
    assert_eq!(summary["pools"], 2);
    assert_eq!(summary["total_bytes"], 130);
    assert_eq!(summary["newest"]["sha"], fx.commits[2]);
    assert_eq!(summary["newest"]["size_bytes"], 123);
    assert_eq!(summary["newest"]["degraded"], false);
    assert_eq!(summary["failed_newer"], 1);
    assert_eq!(summary["failed_latest"]["reason"], ABORT_FLOOR);
    assert_eq!(
        summary["last_skip"]["reason"], SKIP_HEADROOM,
        "a skip newer than the newest pool is a cause"
    );
    assert_eq!(summary["last_reclaim"], Value::Null);
    let keys: Vec<&String> = summary.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "pools",
            "total_bytes",
            "reclaimable_bytes",
            "newest",
            "failed_newer",
            "failed_latest",
            "last_skip",
            "last_reclaim"
        ]
    );
}

#[test]
fn summary_counts_only_idle_pools_on_the_requested_filesystem_and_sizes_an_old_pool_once() {
    let fx = fx(&[]);
    let idle = format!("idle-{}", fx.key);
    let leased = format!("leased-{}", fx.key);
    let mut record = fx.pool(&idle, 0, 1.0, false, 50);
    fx.pool(&leased, 1, 2.0, false, 70);
    // Both markers describe an old pool without a recorded size; the walk caches it.
    for name in [&idle, &leased] {
        let mut marker = fx.state(name).unwrap();
        marker.as_object_mut().unwrap().remove("size_bytes");
        write_marker(&fx.directory.join(format!("{name}.json")), &marker).unwrap();
    }
    let git = fx.git();
    let reader = use_lock(&fx.directory, &leased, false).unwrap();
    let summary = summarise(
        &git,
        std::slice::from_ref(&fx.directory),
        "rolling",
        Some(fx.directory.as_path()),
    );
    assert_eq!(summary["total_bytes"], 120);
    assert_eq!(
        summary["reclaimable_bytes"], 50,
        "the pool a seed reads is not reclaimable"
    );
    drop(reader);
    assert_eq!(
        fx.state(&idle).unwrap()["size_bytes"],
        50,
        "the size is remembered in the marker"
    );
    // A filesystem the pools are not on counts nothing as reclaimable.
    let elsewhere = tempfile::tempdir().unwrap();
    let on_other = summarise(
        &git,
        std::slice::from_ref(&fx.directory),
        "rolling",
        Some(elsewhere.path()),
    );
    let _ = (&mut record, on_other);
}

#[test]
fn a_ready_marker_without_a_target_is_not_ready_and_not_counted() {
    let fx = fx(&[]);
    let name = format!("gone-{}", fx.key);
    fx.pool(&name, 0, 1.0, false, 10);
    fs::remove_dir_all(fx.directory.join(&name).join("target")).unwrap();
    let git = fx.git();
    let summary = summarise(&git, std::slice::from_ref(&fx.directory), "rolling", None);
    assert_eq!(summary["pools"], 0);
    assert_eq!(summary["newest"], Value::Null);
    assert!(ready_pools(&fx.directory, &fx.key).is_empty());
}

#[test]
fn pool_size_cannot_resurrect_a_marker_removed_during_the_walk() {
    let fx = fx(&[]);
    let name = format!("p-{}", fx.key);
    let record = fx.pool(&name, 0, 1.0, false, 10);
    let mut sized = record.clone();
    sized.as_object_mut().unwrap().remove("size_bytes");
    fs::remove_file(fx.directory.join(format!("{name}.json"))).unwrap();
    assert_eq!(pool_size(&fx.directory, &sized), 10);
    assert!(
        !fx.exists(&name),
        "no marker is written for a pool whose marker is gone"
    );
    // A rebuilt generation (another built_at) is not written either.
    let rebuilt = fx.pool(&name, 0, 99.0, false, 10);
    assert_eq!(pool_size(&fx.directory, &sized), 10);
    assert!(fx.state(&name).unwrap().get("size_bytes").is_none());
    let _ = rebuilt;
}

#[test]
fn status_prints_every_marker_of_the_directory_in_name_order() {
    let fx = fx(&[]);
    fx.pool(&format!("b-{}", fx.key), 1, 2.0, false, 1);
    fx.pool(&format!("a-{}", fx.key), 0, 1.0, false, 1);
    record_event(&fx.directory, "skip", fields(json!({"reason": "x"})));
    let mut out = vec![];
    status_command(Some(fx.repo.path()), &fx.env, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "the events file is not a marker: {text}");
    assert!(lines[0].contains(&format!("\"name\": \"a-{}\"", fx.key)));
    assert!(lines[1].contains(&format!("\"name\": \"b-{}\"", fx.key)));
}

// ---- reclaim-idle ----

#[test]
fn reclaim_idle_evicts_unleased_pools_skips_a_leased_one_and_is_bounded() {
    let fx = fx(&[]);
    let idle = format!("idle-{}", fx.key);
    let leased = format!("leased-{}", fx.key);
    fx.pool(&idle, 0, 1.0, false, 4096);
    fx.pool(&leased, 1, 2.0, false, 100);
    let reader = use_lock(&fx.directory, &leased, false).unwrap();
    let run = |interval: f64| -> Value {
        let mut out = vec![];
        reclaim_idle_command(interval, None, &fx.env, &mut out).unwrap();
        serde_json::from_slice(&out).unwrap()
    };
    let first = run(600.0);
    assert_eq!(first["reason"], RECLAIM_FLOOR);
    assert_eq!(first["removed"], json!([idle]));
    assert_eq!(first["skipped_in_use"], json!([leased]));
    assert_eq!(first["freed_bytes"], 4096);
    assert!(!fx.exists(&idle) && fx.exists(&leased));
    let event = read_events(&fx.directory);
    assert_eq!(event["reclaim"]["removed"], 1);
    assert_eq!(event["reclaim"]["freed_bytes"], 4096);
    // Within the interval the directory is left alone.
    drop(reader);
    let second = run(600.0);
    assert_eq!(second["removed"], json!([]));
    assert_eq!(second["throttled"].as_array().unwrap().len(), 1);
    assert!(fx.exists(&leased));
    // An interval of zero reclaims again.
    let third = run(0.0);
    assert_eq!(third["removed"], json!([leased]));
}

#[test]
fn the_last_pool_size_survives_a_reclaim_for_the_next_admission() {
    let fx = fx(&[("RSI_TARGET_POOL_EXPECTED_GIB", "3")]);
    let name = format!("p-{}", fx.key);
    fx.pool(&name, 0, 1.0, false, 8192);
    record_event(
        &fx.directory,
        "last_pool",
        fields(json!({"size_bytes": 8192, "sha": fx.commits[0]})),
    );
    let mut out = vec![];
    reclaim_idle_command(0.0, None, &fx.env, &mut out).unwrap();
    assert!(!fx.exists(&name));
    let (bytes, source) = expected_pool_size(
        &fx.git(),
        &fx.directory,
        fx.repo.path(),
        &fx.commits[1],
        &fx.key,
    );
    assert_eq!((bytes, source), (8192, "last pool built"));
}

// ---- disk floors and admission ----

fn stated_free<T>(gib: f64, f: impl FnOnce() -> T) -> T {
    let _disk = test_disk::Disk::new(Some(gib));
    f()
}

fn stated_disk<T>(free: f64, total: f64, f: impl FnOnce() -> T) -> T {
    let _disk = test_disk::Disk::with_total(Some(free), Some(total));
    f()
}

#[test]
fn the_pool_floor_is_capped_at_a_quarter_of_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let (_, total) = disk_gib(dir.path()).unwrap();
    let floor = scaled_floor_gib(dir.path(), 10.0 * total).unwrap();
    assert!((floor - total / 4.0).abs() < 1e-6, "{floor} vs {total}");
    assert_eq!(scaled_floor_gib(dir.path(), 0.0).unwrap(), 0.0);
}

#[test]
fn the_worker_floor_comes_from_the_daemon_setting_then_cargo_slot_then_thirty() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = Env::new();
    let (_, total) = disk_gib(dir.path()).unwrap();
    let expect = |configured: f64| configured.min(total / 4.0);
    assert!((worker_floor_gib(&env, dir.path()).unwrap() - expect(30.0)).abs() < 1e-6);
    env.insert("RSI_CARGO_MIN_FREE_GB".into(), "12".into());
    assert!((worker_floor_gib(&env, dir.path()).unwrap() - expect(12.0)).abs() < 1e-6);
    env.insert("RSI_TARGET_POOL_WORKER_FLOOR_GIB".into(), "7".into());
    assert!((worker_floor_gib(&env, dir.path()).unwrap() - expect(7.0)).abs() < 1e-6);
    env.insert("RSI_TARGET_POOL_WORKER_FLOOR_GIB".into(), "junk".into());
    assert!(
        (worker_floor_gib(&env, dir.path()).unwrap() - expect(12.0)).abs() < 1e-6,
        "an unreadable setting falls through"
    );
}

#[test]
fn the_expected_size_is_the_nearest_ancestor_then_the_newest_pool_then_the_last_then_the_default() {
    let fx = fx(&[("RSI_TARGET_POOL_EXPECTED_GIB", "2")]);
    let git = fx.git();
    let repo = fx.repo.path();
    let default = (2.0 * GIB) as u64;
    assert_eq!(
        expected_pool_size(&git, &fx.directory, repo, &fx.commits[4], &fx.key),
        (default, "default")
    );
    record_event(
        &fx.directory,
        "last_pool",
        fields(json!({"size_bytes": 999, "sha": "x"})),
    );
    assert_eq!(
        expected_pool_size(&git, &fx.directory, repo, &fx.commits[4], &fx.key),
        (999, "last pool built")
    );
    // Newest pool: not an ancestor of the commit asked about.
    testutil::git(repo, &["checkout", "-q", "-b", "side", &fx.commits[1]]);
    let side = commit(repo, "side.txt", "s\n");
    testutil::git(repo, &["checkout", "-q", "main"]);
    let mut other = fx.pool(&format!("side-{}", fx.key), 0, 3.0, false, 300);
    other["sha"] = json!(side);
    write_marker(&fx.directory.join(format!("side-{}.json", fx.key)), &other).unwrap();
    assert_eq!(
        expected_pool_size(&git, &fx.directory, repo, &fx.commits[4], &fx.key),
        (300, "newest ready pool")
    );
    fx.pool(&format!("near-{}", fx.key), 2, 1.0, false, 200);
    assert_eq!(
        expected_pool_size(&git, &fx.directory, repo, &fx.commits[4], &fx.key),
        (200, "nearest ready ancestor")
    );
}

#[test]
fn admission_needs_the_floor_plus_the_expected_pool_size() {
    let fx = fx(&[
        ("RSI_TARGET_POOL_EXPECTED_GIB", "10"),
        ("RSI_TARGET_POOL_WORKER_FLOOR_GIB", "0"),
    ]);
    let git = fx.git();
    let ask = |free: f64, floor: f64| {
        stated_disk(free, 400.0, || {
            super::admission_skip_with_expected(
                &git,
                &fx.directory,
                &fx.commits[1],
                floor,
                ((10.0 * GIB) as u64, "default"),
            )
            .unwrap()
        })
    };
    assert!(ask(1000.0, 40.0).is_none(), "plenty of room");
    let skip = ask(45.0, 40.0).expect("45 GiB cannot cover 40 + 10");
    assert_eq!(skip["reason"], SKIP_HEADROOM);
    assert_eq!(skip["needed_gib"], 50.0);
    assert_eq!(skip["floor_gib"], 40.0);
    assert_eq!(skip["expected_gib"], 10.0);
    assert_eq!(skip["expected_source"], "default");
    assert!(skip["detail"].as_str().unwrap().starts_with(
        "only 45.0 GiB free; needs 50.0 GiB (floor 40.0 + expected pool 10.0, default)"
    ));
    assert!(ask(50.0, 40.0).is_none(), "exactly enough");
    assert!(ask(0.0, 0.0).is_none(), "a floor of 0 turns the check off");
}

#[test]
fn the_workers_floor_counts_when_it_is_above_the_pool_floor() {
    let fx = fx(&[
        ("RSI_TARGET_POOL_EXPECTED_GIB", "10"),
        ("RSI_TARGET_POOL_WORKER_FLOOR_GIB", "100"),
    ]);
    let git = fx.git();
    let (_, total) = disk_gib(&fx.directory).unwrap();
    if total < 440.0 {
        return; // the worker floor is capped at a quarter of this small disk
    }
    let skip = stated_free(105.0, || {
        admission_skip(
            &git,
            &fx.directory,
            fx.repo.path(),
            &fx.commits[1],
            &fx.key,
            40.0,
        )
        .unwrap()
    })
    .expect("the 100 GiB worker floor plus 10 GiB needs 110");
    assert_eq!(skip["floor_gib"], 100.0);
    assert_eq!(skip["needed_gib"], 110.0);
}

#[test]
fn events_are_remembered_per_kind_and_forgotten_by_none() {
    let fx = fx(&[]);
    record_event(&fx.directory, "skip", fields(json!({"reason": "r"})));
    record_event(&fx.directory, "last_pool", fields(json!({"size_bytes": 1})));
    let events = read_events(&fx.directory);
    assert_eq!(events["skip"]["reason"], "r");
    assert!(events["skip"]["at"].as_f64().unwrap() > 1.0e9);
    record_event(&fx.directory, "skip", None);
    let events = read_events(&fx.directory);
    assert!(events.get("skip").is_none() && events.get("last_pool").is_some());
    // A damaged events file reads as empty and is replaced, never fatal.
    fs::write(fx.directory.join(EVENTS_FILE), "{ not json").unwrap();
    assert!(read_events(&fx.directory).is_empty());
    record_event(&fx.directory, "skip", fields(json!({"reason": "again"})));
    assert_eq!(read_events(&fx.directory)["skip"]["reason"], "again");
}

#[test]
fn markers_are_written_atomically_without_leaving_a_temp_name_that_matches_json() {
    let fx = fx(&[]);
    let marker = fx.directory.join("m.json");
    write_marker(&marker, &json!({"a": 1})).unwrap();
    assert_eq!(fs::read_to_string(&marker).unwrap(), "{\"a\": 1}\n");
    let leftovers: Vec<_> = fs::read_dir(&fx.directory)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

// ---- refresh ----

fn refresh_with(fx: &Fx, repo: Option<&Path>, retry_after: f64) -> (Value, Vec<String>) {
    let launched = std::cell::RefCell::new(vec![]);
    let launch = |_: &Git, _: &Path, tip: &str| -> io::Result<String> {
        launched.borrow_mut().push(tip.to_string());
        Ok("started".to_string())
    };
    let mut out = vec![];
    refresh_command(
        &RefreshOptions {
            repo,
            reference: "rolling",
            max_distance: 2,
            retry_after,
        },
        &fx.env,
        &launch,
        &mut out,
    )
    .unwrap();
    (serde_json::from_slice(&out).unwrap(), launched.into_inner())
}

#[test]
fn the_nearest_pool_triggers_a_refresh_past_half_the_distance_limit() {
    assert!(refresh_needed(None, 200));
    assert!(!refresh_needed(Some(100), 200));
    assert!(refresh_needed(Some(101), 200));
    assert!(!refresh_needed(Some(0), 0));
    assert!(refresh_needed(Some(1), 0));
}

#[test]
fn refresh_builds_at_the_tip_when_the_only_pool_was_collected_as_stale() {
    let fx = fx(&[]);
    let tip = fx.commits[4].clone();
    testutil::git(fx.repo.path(), &["branch", "-f", "rolling", "HEAD"]);
    remember_repo(&fx.git(), &fx.directory, fx.repo.path());
    // Nothing is left to name the checkout but the remembered repo file; no pool at all.
    let (result, launched) = refresh_with(&fx, None, 3600.0);
    assert_eq!(launched, [tip.clone()]);
    let entry = &result["refresh"][0];
    assert_eq!(entry["tip"], tip);
    assert_eq!(entry["nearest"], Value::Null);
    assert_eq!(entry["result"], "started");
    assert_eq!(
        entry["directory"],
        fx.directory.file_name().unwrap().to_string_lossy().as_ref()
    );
}

#[test]
fn refresh_leaves_a_fresh_pool_and_a_recent_failure_alone() {
    let fx = fx(&[]);
    testutil::git(fx.repo.path(), &["branch", "-f", "rolling", "HEAD"]);
    fx.pool(&format!("near-{}", fx.key), 3, 1.0, false, 10);
    let (result, launched) = refresh_with(&fx, Some(fx.repo.path()), 3600.0);
    assert!(launched.is_empty());
    assert_eq!(result["refresh"][0]["result"], "fresh");
    assert_eq!(result["refresh"][0]["nearest"], 1);
    // A pool far behind the tip wants a refresh, unless the tip's build failed recently.
    fs::remove_file(fx.directory.join(format!("near-{}.json", fx.key))).unwrap();
    fx.pool(&format!("far-{}", fx.key), 0, 1.0, false, 10);
    write_marker(
        &fx.directory
            .join(format!("{}-{}.json", fx.commits[4], fx.key)),
        &json!({"name": "x", "sha": fx.commits[4], "state": "failed", "built_at": now()}),
    )
    .unwrap();
    let (result, launched) = refresh_with(&fx, Some(fx.repo.path()), 3600.0);
    assert!(launched.is_empty());
    assert_eq!(
        result["refresh"][0]["result"],
        "skipped: the build of this tip failed recently"
    );
    let (_, launched) = refresh_with(&fx, Some(fx.repo.path()), 0.0);
    assert_eq!(
        launched,
        [fx.commits[4].clone()],
        "an old failure is retried"
    );
}

#[test]
fn refresh_without_a_known_checkout_or_ref_says_so_and_builds_nothing() {
    let fx = fx(&[]);
    let (result, launched) = refresh_with(&fx, None, 3600.0);
    assert!(launched.is_empty());
    assert_eq!(
        result["refresh"][0]["result"],
        "skipped: no checkout is known for these pools"
    );
    let (result, launched) = refresh_with(&fx, Some(fx.repo.path()), 3600.0);
    assert!(launched.is_empty());
    assert_eq!(
        result["refresh"][0]["result"],
        "skipped: rolling does not resolve"
    );
}

// ---- the byte-compatible helpers ----

#[test]
fn tree_size_counts_each_hard_linked_inode_once_and_skips_symlinked_directories() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
    fs::hard_link(dir.path().join("a"), dir.path().join("sub/a-link")).unwrap();
    fs::write(dir.path().join("sub/b"), vec![0u8; 50]).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("sub"), dir.path().join("dir-link")).unwrap();
        assert_eq!(tree_size(dir.path()), 150);
    }
}

#[test]
fn rounding_and_float_helpers_match_the_python_tool() {
    assert_eq!(round1(2.25), 2.2, "ties go to even, as in Python");
    assert_eq!(round1(2.45), 2.5, "2.45 is just above the tie");
    assert_eq!(round1(30.04), 30.0);
}

// ---- the interleavings and command lines the legacy suite ran (#453, case map in
// `target_pool_case_map.rs`) ----

/// An exclusive, non-blocking `flock` on a descriptor opened earlier: what a gc that
/// planned before another gc, a rebuild and a seed does when it finally locks.
#[cfg(unix)]
fn try_exclusive(file: &fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor `file` owns for the call.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(unix)]
fn open_lock_file(path: &Path) -> fs::File {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
        .unwrap()
}

/// A ready pool whose marker names `worktree` (a real checkout, so Git can count its
/// distance from a ref).
fn pool_in_checkout(fx: &Fx, name: &str, index: usize, built_at: f64) -> Value {
    let record = fx.pool(name, index, built_at, false, 10);
    let mut record = record;
    record["worktree"] = json!(fx.repo.path());
    write_marker(&fx.directory.join(format!("{name}.json")), &record).unwrap();
    record
}

#[cfg(unix)]
#[test]
fn a_stale_gc_cannot_delete_a_pool_rebuilt_and_seeded_meanwhile() {
    let fx = fx(&[]);
    let names: Vec<String> = (0..3).map(|n| format!("s{n}-{}", fx.key)).collect();
    for (n, name) in names.iter().enumerate() {
        fx.pool(name, n, 100.0 + n as f64, false, 10);
    }
    let a = &names[0];
    // GC1 has opened A's lock file ...
    let stale = open_lock_file(&fx.directory.join(format!("{a}.use")));
    // ... GC2 removes A ...
    let (removed, _) = fx.gc(&plain(1));
    assert!(removed.contains(a), "{removed:?}");
    assert!(
        fx.directory.join(format!("{a}.use")).exists(),
        "the lock file stays, so its inode is the one every later locker sees"
    );
    // ... a build recreates A and a seed locks it ...
    fx.pool(a, 0, 500.0, false, 10);
    let seed = use_lock(&fx.directory, a, false).expect("shared lock on the rebuilt pool");
    // ... and GC1 resumes: its descriptor is the seed's lock, so it is refused.
    assert!(
        !try_exclusive(&stale),
        "a stale gc must see the seed's lock"
    );
    // Control: had the lock file been unlinked with the pool and recreated, the stale
    // descriptor would name a different inode and conflict with nothing.
    let control = fx.directory.join("control.use");
    let old = open_lock_file(&control);
    fs::remove_file(&control).unwrap();
    let fresh = open_lock_file(&control);
    assert!(use_lock(&fx.directory, "control", false).is_some());
    assert!(
        try_exclusive(&old),
        "the control detects an unlinked lock file"
    );
    drop(fresh);
    // The rebuilt pool is whole.
    assert!(
        fx.directory
            .join(a)
            .join("target/debug/deps/a.rlib")
            .is_file()
    );
    assert_eq!(fx.state(a).unwrap()["built_at"], 500.0);
    drop(seed);
}

#[test]
fn a_stale_gc_does_not_remove_a_pool_rebuilt_since_its_plan() {
    let fx = fx(&[]);
    let names: Vec<String> = (0..3).map(|n| format!("s{n}-{}", fx.key)).collect();
    for (n, name) in names.iter().enumerate() {
        fx.pool(name, n, 100.0 + n as f64, false, 10);
    }
    let a = names[0].clone();
    let mut done = false;
    let mut skipped = vec![];
    let removed = gc_with(
        &fx.git(),
        &fx.directory,
        &plain(1),
        &mut skipped,
        &mut |name| {
            if name == a && !done {
                done = true;
                // Same name, new generation, between the plan and the lock.
                fs::remove_dir_all(fx.directory.join(&a)).unwrap();
                fx.pool(&a, 0, 500.0, false, 10);
            }
        },
    );
    assert!(!removed.contains(&a), "{removed:?}");
    assert!(
        fx.directory
            .join(&a)
            .join("target/debug/deps/a.rlib")
            .is_file()
    );
    let marker = fx.state(&a).unwrap();
    assert_eq!(
        (marker["state"].as_str(), marker["built_at"].as_f64()),
        (Some("ready"), Some(500.0))
    );
}

#[test]
fn gc_never_removes_a_stale_pool_a_seed_is_reading() {
    let fx = fx(&[]);
    testutil::git(fx.repo.path(), &["branch", "-f", "rolling", "HEAD"]);
    let name = format!("old-{}", fx.key);
    pool_in_checkout(&fx, &name, 0, 100.0); // four commits behind the tip
    let reader = use_lock(&fx.directory, &name, false).unwrap();
    let options = GcOptions {
        reference: Some("rolling"),
        max_distance: 2,
        repo: Some(fx.repo.path()),
        ..plain(2)
    };
    let (removed, skipped) = fx.gc(&options);
    assert_eq!((removed, skipped), (vec![], vec![name.clone()]));
    assert!(
        fx.directory
            .join(&name)
            .join("target/debug/deps/a.rlib")
            .is_file()
    );
    drop(reader);
    assert_eq!(
        fx.gc(&options).0,
        [name],
        "once the reader is done the stale pool goes"
    );
}

#[test]
fn the_gc_command_collects_a_stale_pool_unless_the_ref_is_emptied() {
    let fx = fx(&[]);
    let name = format!("old-{}", fx.key);
    let ctx = crate::Ctx::new(fx.repo.path(), &[]);
    let run = |extra: &[&str]| -> (i32, Value, String) {
        let args: Vec<String> = ["--json"]
            .iter()
            .chain(extra)
            .map(|a| (*a).to_string())
            .collect();
        let (mut out, mut err) = (vec![], vec![]);
        let code = crate::target_pool_cli::gc(&args, &ctx, &fx.env, &mut out, &mut err).unwrap();
        let text = String::from_utf8(out).unwrap();
        (
            code,
            serde_json::from_str(text.trim()).unwrap_or(Value::Null),
            String::from_utf8(err).unwrap(),
        )
    };
    pool_in_checkout(&fx, &name, 0, 100.0);
    let (code, report, err) = run(&["--ref", "main", "--max-distance", "2"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(report["removed"], json!([name]));
    pool_in_checkout(&fx, &name, 0, 100.0);
    let (code, report, err) = run(&["--ref", "", "--max-distance", "1"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        report["removed"],
        json!([]),
        "an emptied ref turns the stale sweep off"
    );
    assert!(fx.exists(&name));
    // A malformed number is argparse's usage error, not a panic.
    let (code, _, err) = run(&["--max-distance", "many"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("error: argument --max-distance: invalid value: \"many\""),
        "{err}"
    );
}

#[test]
fn a_pool_directory_remembers_its_checkout_before_the_last_pool_goes() {
    let fx = fx(&[]);
    let name = format!("old-{}", fx.key);
    pool_in_checkout(&fx, &name, 0, 100.0);
    assert!(known_repo(&fx.directory).is_none());
    let options = GcOptions {
        reference: Some("main"),
        max_distance: 1,
        ..plain(2)
    };
    let (removed, _) = fx.gc(&options);
    assert_eq!(removed, [name]);
    assert_eq!(
        known_repo(&fx.directory).map(|p| fs::canonicalize(p).unwrap()),
        Some(fs::canonicalize(fx.repo.path()).unwrap())
    );
}

#[test]
fn a_stale_imported_pool_is_collected_then_refreshed_without_a_repo() {
    let fx = fx(&[]);
    testutil::git(fx.repo.path(), &["branch", "-f", "rolling", "HEAD"]);
    let name = format!("imp-{}", fx.key);
    let mut record = fx.pool(&name, 0, 100.0, false, 10);
    record["seeded_from"] = json!({"host": "peer"}); // a pool seed-from made: no Git worktree around it
    record["worktree"] = json!(fx.directory.join("not-a-checkout"));
    write_marker(&fx.directory.join(format!("{name}.json")), &record).unwrap();
    assert!(known_repo(&fx.directory).is_none());
    let options = GcOptions {
        reference: Some("rolling"),
        max_distance: 2,
        ..plain(2)
    };
    // Before a seed recorded the checkout: nothing is collectable and nothing refreshes.
    assert!(fx.gc(&options).0.is_empty());
    let (result, launched) = refresh_with(&fx, None, 3600.0);
    assert!(launched.is_empty());
    assert!(
        result["refresh"][0]["result"]
            .as_str()
            .unwrap()
            .starts_with("skipped: no checkout"),
        "{result}"
    );
    // A worker seed in a sandbox of the repository records the checkout.
    let (mut out, mut err) = (vec![], vec![]);
    let code = crate::target_pool::main(
        &[
            "seed".into(),
            "--sandbox".into(),
            fx.repo.path().to_string_lossy().into_owned(),
            "--max-distance".into(),
            "1".into(),
        ],
        &crate::Ctx::new(
            fx.repo.path(),
            &fx.env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
        ),
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert!(known_repo(&fx.directory).is_some());
    let removed = fx
        .gc(&GcOptions {
            repo: None,
            ..options
        })
        .0;
    assert_eq!(removed, [name.clone()]);
    assert!(!fx.directory.join(&name).exists());
    assert!(ready_pools(&fx.directory, &fx.key).is_empty());
    let (result, launched) = refresh_with(&fx, None, 3600.0);
    assert_eq!(launched, [fx.commits[4].clone()]);
    assert_eq!(result["refresh"][0]["result"], "started");
    assert_eq!(result["refresh"][0]["nearest"], Value::Null);
}

#[test]
fn summary_and_gc_leave_a_pool_alone_on_a_filesystem_they_were_not_asked_about() {
    let fx = fx(&[]);
    let idle = format!("idle-{}", fx.key);
    let busy = format!("busy-{}", fx.key);
    fx.pool(&idle, 0, 1.0, false, 200);
    fx.pool(&busy, 1, 2.0, false, 300);
    let reader = use_lock(&fx.directory, &busy, false).unwrap();
    let git = fx.git();
    let here = summarise(
        &git,
        std::slice::from_ref(&fx.directory),
        "main",
        Some(fx.directory.as_path()),
    );
    assert_eq!(here["total_bytes"], 500);
    assert_eq!(here["reclaimable_bytes"], 200);
    // A filesystem the pools are not on (here: one that does not exist) reclaims nothing.
    let nowhere = fx.directory.join("no-such-filesystem");
    let there = summarise(
        &git,
        std::slice::from_ref(&fx.directory),
        "main",
        Some(nowhere.as_path()),
    );
    assert_eq!(there["reclaimable_bytes"], 0);
    let (removed, skipped) = fx.gc(&GcOptions {
        filesystem: Some(nowhere.as_path()),
        ..plain(0)
    });
    assert!(
        removed.is_empty() && skipped.is_empty(),
        "{removed:?} {skipped:?}"
    );
    assert!(
        fx.directory
            .join(&idle)
            .join("target/debug/deps/a.rlib")
            .is_file()
    );
    assert!(
        fx.directory
            .join(&busy)
            .join("target/debug/deps/a.rlib")
            .is_file()
    );
    drop(reader);
    let all = summarise(&git, std::slice::from_ref(&fx.directory), "main", None);
    assert_eq!(all["reclaimable_bytes"], 500);
}

#[test]
fn pool_size_yields_while_gc_holds_the_pool_and_writes_nothing() {
    let fx = fx(&[]);
    let name = format!("p-{}", fx.key);
    let mut record = fx.pool(&name, 0, 1.0, false, 10);
    record.as_object_mut().unwrap().remove("size_bytes");
    let removing = use_lock(&fx.directory, &name, true).expect("gc's exclusive lock");
    assert_eq!(
        pool_size(&fx.directory, &record),
        0,
        "gc is removing it: no walk, no cache write"
    );
    assert!(fx.state(&name).unwrap().get("size_bytes").is_none());
    drop(removing);
    let probe = use_lock(&fx.directory, &name, false);
    assert!(
        probe.is_some(),
        "gc let go of the pool, so a shared lock is free"
    );
    drop(probe);
    assert!(Path::new(record["target"].as_str().unwrap()).is_dir());
    assert_eq!(pool_size(&fx.directory, &record), 10);
    assert_eq!(fx.state(&name).unwrap()["size_bytes"], 10);
}

#[test]
fn a_skipped_request_is_history_once_a_newer_pool_exists() {
    let fx = fx(&[
        ("RSI_TARGET_POOL_WORKER_FLOOR_GIB", "1"),
        ("RSI_TARGET_POOL_MIN_FREE_GIB", "2"),
    ]);
    record_event(
        &fx.directory,
        "skip",
        fields(json!({"reason": SKIP_HEADROOM, "needed_gib": 18.0, "free_gib": 5.0})),
    );
    let git = fx.git();
    let before = summarise(&git, std::slice::from_ref(&fx.directory), "main", None);
    assert_eq!(before["last_skip"]["reason"], SKIP_HEADROOM);
    assert_eq!(before["last_skip"]["needed_gib"], 18.0);
    assert_eq!(before["last_skip"]["free_gib"], 5.0);
    // Once a newer pool exists the skip is history, not the reason there is no pool.
    fx.pool(&format!("new-{}", fx.key), 0, now() + 10.0, false, 10);
    let after = summarise(&git, std::slice::from_ref(&fx.directory), "main", None);
    assert_eq!(after["last_skip"], Value::Null);
}

#[test]
fn gc_pressure_keeps_the_last_complete_pool_under_a_newer_degraded_one() {
    let fx = fx(&[]);
    let complete = format!("s0-{}", fx.key);
    let degraded = format!("s1-{}", fx.key);
    fx.pool(&complete, 0, 100.0, false, 10);
    fx.pool(&degraded, 1, 101.0, true, 10);
    let (removed, skipped) = fx.gc(&GcOptions {
        pressure: true,
        ..plain(2)
    });
    assert!(
        removed.is_empty() && skipped.is_empty(),
        "{removed:?} {skipped:?}"
    );
    assert!(fx.exists(&complete) && fx.exists(&degraded));
    assert!(
        fx.directory
            .join(&complete)
            .join("target/debug/deps/a.rlib")
            .is_file()
    );
}

#[test]
fn an_explicit_zero_evicts_the_last_complete_pool_only_after_its_reader_finishes_under_pressure_too()
 {
    for pressure in [false, true] {
        let fx = fx(&[]);
        let complete = format!("complete-{}", fx.key);
        let degraded = format!("degraded-{}", fx.key);
        fx.pool(&complete, 0, 100.0, false, 10);
        fx.pool(&degraded, 1, 200.0, true, 10);
        write_marker(
            &fx.directory.join(format!("failed-{}.json", fx.key)),
            &json!({"name": format!("failed-{}", fx.key), "state": "failed", "built_at": 300.0}),
        )
        .unwrap();
        // Another repository's pool directory under the same root is not this command's.
        let other = PathBuf::from(fx.env["RSI_TARGET_POOL_ROOT"].as_str()).join("other-repo");
        fs::create_dir_all(other.join("other-k/target")).unwrap();
        write_marker(
            &other.join("other-k.json"),
            &json!({"name": "other-k", "sha": fx.commits[0], "key": "k", "state": "ready",
                    "target": other.join("other-k/target"), "built_at": 400.0}),
        )
        .unwrap();
        let reader = use_lock(&fx.directory, &complete, false).expect("shared lock");
        let command = || {
            let mut out = vec![];
            let options = GcCommand {
                repo: Some(fx.repo.path()),
                keep: Some(0),
                pressure,
                json: true,
                reference: None,
                max_distance: 200,
            };
            assert_eq!(gc_command(&options, &fx.env, &mut out).unwrap(), 0);
            serde_json::from_slice::<Value>(&out).unwrap()
        };
        let report = command();
        assert_eq!(report["removed"], json!([degraded]), "pressure={pressure}");
        assert_eq!(
            report["skipped_in_use"],
            json!([complete]),
            "pressure={pressure}"
        );
        assert!(
            fx.directory
                .join(&complete)
                .join("target/debug/deps/a.rlib")
                .is_file()
        );
        drop(reader);
        assert_eq!(
            command(),
            json!({"removed": [complete], "skipped_in_use": []}),
            "pressure={pressure}"
        );
        assert!(
            fx.exists(&format!("failed-{}", fx.key)),
            "a failed marker is history, not a pool"
        );
        assert!(!fx.exists(&complete) && !fx.exists(&degraded));
        assert!(other.join("other-k.json").is_file() && other.join("other-k/target").is_dir());
    }
}

#[test]
fn stale_pool_names_are_empty_for_a_ref_that_does_not_resolve() {
    let fx = fx(&[]);
    let record =
        json!({"name": "far", "state": "ready", "sha": fx.commits[0], "worktree": fx.repo.path()});
    let git = fx.git();
    assert_eq!(
        stale_pool_names(&git, std::slice::from_ref(&record), "main", 1, None),
        BTreeSet::from(["far".to_string()])
    );
    assert!(
        stale_pool_names(&git, std::slice::from_ref(&record), "no-such-ref", 0, None).is_empty()
    );
}

#[test]
fn the_workers_floor_counts_when_it_is_above_the_pool_floor_on_any_disk() {
    // The floors are capped at a quarter of the disk, so state them as fractions of this
    // one: the worker floor (an eighth) above the pool floor (a sixteenth), both uncapped.
    let probe = tempfile::tempdir().unwrap();
    let (_, total) = disk_gib(probe.path()).unwrap();
    let worker = (total / 8.0).floor();
    if worker < 4.0 {
        return; // a disk this small cannot state two distinct floors
    }
    let pool_floor = (worker / 2.0).floor();
    let fx = fx(&[
        ("RSI_TARGET_POOL_EXPECTED_GIB", "1"),
        ("RSI_TARGET_POOL_WORKER_FLOOR_GIB", &worker.to_string()),
    ]);
    let git = fx.git();
    let skip = stated_free(worker + 0.5, || {
        admission_skip(
            &git,
            &fx.directory,
            fx.repo.path(),
            &fx.commits[1],
            &fx.key,
            pool_floor,
        )
        .unwrap()
    })
    .expect("the worker floor plus the expected pool does not fit");
    assert_eq!(skip["floor_gib"], worker);
    assert_eq!(skip["needed_gib"], worker + 1.0);
    // A floor of zero turns the check off.
    assert!(
        stated_free(0.0, || admission_skip(
            &git,
            &fx.directory,
            fx.repo.path(),
            &fx.commits[1],
            &fx.key,
            0.0
        )
        .unwrap())
        .is_none()
    );
}
