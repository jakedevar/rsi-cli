//! Every test builds a synthetic `<tmp home>/.rsi/<trash>`; the operator's
//! `~/.rsi` is never touched.

use super::*;
use chrono::{Duration as Age, TimeZone};
use std::fs;
use std::path::PathBuf;

fn days(n: i64) -> Age {
    Age::days(n)
}

struct Home {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    trash: PathBuf,
}

impl Home {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let trash = home.join(".rsi").join("project-file-trash");
        fs::create_dir_all(&trash).unwrap();
        Self {
            _tmp: tmp,
            home,
            trash,
        }
    }

    /// A batch recorded `age` before `now` holding `<name>/inner.txt`, with
    /// its sidecar.
    fn batch(&self, now: DateTime<Utc>, age: Age, name: &str) -> String {
        let batch = new_batch_name(now - age);
        let dir = self.trash.join(&batch).join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("inner.txt"), "data").unwrap();
        fs::write(
            self.trash.join(format!("{batch}.{ORIGIN_EXTENSION}")),
            "/project/src",
        )
        .unwrap();
        batch
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.trash)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap()
}

#[test]
fn batch_names_record_their_utc_time_and_other_names_record_none() {
    let at = now();
    let name = new_batch_name(at);
    assert_eq!(name.len(), 24);
    assert_eq!(batch_recorded_at(&name), Some(at));
    assert_eq!(
        batch_recorded_at("20261019T120000-0a1b2c3d"),
        Some(Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap())
    );
    for foreign in [
        "old",
        "notes-0a1b2c3d",
        "20261019T120000-xyzxyzxy",
        "20261019T120000-0a1b",
        "20261319T120000-0a1b2c3d",
    ] {
        assert_eq!(batch_recorded_at(foreign), None, "{foreign}");
    }
    assert!(is_expired(at - days(8), at, TRASH_MAX_AGE));
    assert!(!is_expired(at - days(7), at, TRASH_MAX_AGE));
    assert!(!is_expired(at + days(1), at, TRASH_MAX_AGE));
}

#[test]
fn only_batches_older_than_seven_days_are_pruned_with_their_sidecars() {
    let home = Home::new();
    let now = now();
    let old = home.batch(now, days(8), "old.rs");
    let very_old = home.batch(now, days(400), "src");
    let young = home.batch(now, days(6), "young.rs");
    let edge = home.batch(now, days(7) - Age::seconds(1), "edge.rs");
    let future = home.batch(now, -days(2), "skew.rs");
    // A foreign directory and an orphan old sidecar.
    fs::create_dir_all(home.trash.join("notes")).unwrap();
    fs::write(home.trash.join("notes").join("keep.txt"), "mine").unwrap();
    let orphan = format!("{}.{ORIGIN_EXTENSION}", new_batch_name(now - days(9)));
    fs::write(home.trash.join(&orphan), "/gone").unwrap();

    let report = prune_trash(&home.trash, TRASH_MAX_AGE, now).unwrap();

    assert_eq!(report.batches_removed, 2);
    assert_eq!(report.sidecars_removed, 3);
    assert_eq!(report.failed, 0);
    assert!(!report.truncated);
    let mut expected = vec![
        young.clone(),
        format!("{young}.origin"),
        edge.clone(),
        format!("{edge}.origin"),
        future.clone(),
        format!("{future}.origin"),
        "notes".to_string(),
    ];
    expected.sort();
    assert_eq!(home.names(), expected);
    assert_eq!(
        fs::read_to_string(home.trash.join(&young).join("young.rs/inner.txt")).unwrap(),
        "data"
    );
    assert_eq!(
        fs::read_to_string(home.trash.join("notes/keep.txt")).unwrap(),
        "mine"
    );
    let _ = (old, very_old);
}

#[test]
fn a_missing_trash_root_is_an_empty_pass() {
    let home = Home::new();
    let report = prune_trash(&home.home.join("absent"), TRASH_MAX_AGE, now()).unwrap();
    assert_eq!(report, PruneReport::default());
}

#[cfg(unix)]
#[test]
fn pruning_never_follows_a_symlink_out_of_the_trash() {
    use std::os::unix::fs::symlink;
    let home = Home::new();
    let now = now();
    // Data outside the trash root, in the synthetic home.
    let outside = home.home.join("outside");
    fs::create_dir_all(outside.join("deep")).unwrap();
    fs::write(outside.join("keep.txt"), "outside").unwrap();
    fs::write(outside.join("deep/keep.txt"), "deeper").unwrap();

    // An old batch whose trashed entries are links pointing out.
    let old = home.batch(now, days(8), "dir");
    symlink(&outside, home.trash.join(&old).join("link-dir")).unwrap();
    symlink(
        outside.join("keep.txt"),
        home.trash.join(&old).join("link-file"),
    )
    .unwrap();
    // Old batch and sidecar names that are themselves links out.
    let linked_batch = new_batch_name(now - days(30));
    symlink(&outside, home.trash.join(&linked_batch)).unwrap();
    let linked_sidecar = format!("{}.{ORIGIN_EXTENSION}", new_batch_name(now - days(30)));
    symlink(outside.join("keep.txt"), home.trash.join(&linked_sidecar)).unwrap();

    let report = prune_trash(&home.trash, TRASH_MAX_AGE, now).unwrap();

    assert_eq!(report.batches_removed, 1);
    assert_eq!(report.sidecars_removed, 1);
    let mut expected = vec![linked_batch.clone(), linked_sidecar];
    expected.sort();
    assert_eq!(home.names(), expected);
    assert!(
        fs::symlink_metadata(home.trash.join(&linked_batch))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "outside"
    );
    assert_eq!(
        fs::read_to_string(outside.join("deep/keep.txt")).unwrap(),
        "deeper"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_trash_root_is_refused_and_its_target_kept() {
    use std::os::unix::fs::symlink;
    let home = Home::new();
    let now = now();
    let elsewhere = home.home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let batch = new_batch_name(now - days(30));
    fs::create_dir_all(elsewhere.join(&batch)).unwrap();
    let link = home.home.join(".rsi").join("explorer-trash");
    symlink(&elsewhere, &link).unwrap();

    assert!(prune_trash(&link, TRASH_MAX_AGE, now).is_err());
    assert!(elsewhere.join(&batch).is_dir());
}
