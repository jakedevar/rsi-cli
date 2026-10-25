//! Hub #3315: the built-in, permanent "RSI home" project.

use rsi_common::types::Project;
use uuid::Uuid;

use super::Store;
use crate::error::DaemonError;

fn refusal(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn homes(store: &Store) -> Vec<Project> {
    store
        .load_projects()
        .unwrap()
        .into_iter()
        .filter(|project| project.id == Project::HOME_ID)
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_home_project_is_registered_once_however_often_the_daemon_starts() {
    let store = Store::open_in_memory().unwrap();
    let home = tempfile::tempdir().unwrap();
    let before = store.load_projects().unwrap().len();
    for _ in 0..4 {
        let project = store.ensure_home_project(home.path()).unwrap();
        assert_eq!(project.id, Project::HOME_ID);
    }
    let homes = homes(&store);
    assert_eq!(homes.len(), 1);
    assert_eq!(homes[0].name, Project::HOME_NAME);
    assert_eq!(homes[0].path.as_deref(), Some(home.path()));
    assert_eq!(store.load_projects().unwrap().len(), before + 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn a_taken_name_and_a_moved_home_still_leave_one_home_project() {
    let store = Store::open_in_memory().unwrap();
    let operator_dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    store
        .insert_project(&Project {
            id: Uuid::new_v4(),
            name: Project::HOME_NAME.into(),
            path: Some(operator_dir.path().to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    let first_home = tempfile::tempdir().unwrap();
    let made = store.ensure_home_project(first_home.path()).unwrap();
    assert_eq!(made.name, format!("{} (2)", Project::HOME_NAME));
    let second_home = tempfile::tempdir().unwrap();
    let moved = store.ensure_home_project(second_home.path()).unwrap();
    assert_eq!(moved.id, Project::HOME_ID);
    assert_eq!(moved.name, made.name);
    let homes = homes(&store);
    assert_eq!(homes.len(), 1);
    assert_eq!(homes[0].path.as_deref(), Some(second_home.path()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn the_home_project_cannot_be_deleted_or_repathed_and_is_never_matched_by_path() {
    let store = Store::open_in_memory().unwrap();
    let home = tempfile::tempdir().unwrap();
    store.ensure_home_project(home.path()).unwrap();

    assert_eq!(
        refusal(store.delete_project(Project::HOME_ID).unwrap_err()),
        Project::HOME_DELETE_REFUSED
    );
    let mut repathed = store.get_project(Project::HOME_ID).unwrap().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    repathed.path = Some(elsewhere.path().to_path_buf());
    assert_eq!(
        refusal(store.update_project(&repathed).unwrap_err()),
        Project::HOME_REPATH_REFUSED
    );
    repathed.path = None;
    assert_eq!(
        refusal(store.update_project(&repathed).unwrap_err()),
        Project::HOME_REPATH_REFUSED
    );

    // A rename keeps the path and is an ordinary edit.
    let mut renamed = store.get_project(Project::HOME_ID).unwrap().unwrap();
    renamed.name = "Home base".into();
    store.update_project(&renamed).unwrap();
    let stored = homes(&store);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].name, "Home base");
    assert_eq!(stored[0].path.as_deref(), Some(home.path()));

    // A directory under `$HOME` that no other project registers stays
    // unscoped instead of joining the home project.
    let inside = home.path().join("scratch");
    assert_eq!(
        store
            .find_project_for_path(&inside)
            .unwrap()
            .map(|project| project.id),
        None
    );
}
