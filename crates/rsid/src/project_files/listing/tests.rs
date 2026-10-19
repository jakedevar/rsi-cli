use super::*;
use crate::project_files::tests::{git, refusal_code, root, write};
use std::cell::RefCell;

thread_local! {
    pub(super) static AFTER_RESOLVE: RefCell<Option<Box<dyn FnOnce() + Send>>> = RefCell::new(None);
    static BEFORE_METADATA: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    static BEFORE_DESCEND: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

pub(super) fn after_resolve() {
    AFTER_RESOLVE.with(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
}
pub(super) fn before_metadata() {
    BEFORE_METADATA.with(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
}
pub(super) fn before_descend() {
    BEFORE_DESCEND.with(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
}

#[cfg(unix)]
fn swap(path: &Path, outside: &Path) {
    std::fs::rename(path, path.with_extension("pinned")).unwrap();
    std::os::unix::fs::symlink(outside, path).unwrap();
}

#[cfg(unix)]
fn replace_after_resolve(path: PathBuf, outside: PathBuf) {
    AFTER_RESOLVE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || swap(&path, &outside))));
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn directory_listing_refuses_root_and_ancestor_symlink_replacements() {
    for replaced_part in ["container", "container/project", "container/project/parent"] {
        let (_temp, root) = root();
        let (_outside, outside) = super::super::tests::root();
        write(
            &root,
            "container/project/parent/child/inside.txt",
            b"inside",
        );
        write(&outside, "project/parent/child/outside.txt", b"outside");
        write(&outside, "parent/child/outside.txt", b"outside metadata");
        write(&outside, "child/outside.txt", b"outside metadata");
        let project = root.join("container/project");
        let replaced = root.join(replaced_part);
        replace_after_resolve(replaced, outside);
        let error = list_directory(project, "parent/child".into(), None)
            .await
            .unwrap_err();
        assert!(matches!(
            refusal_code(error).as_str(),
            write::IO_FAILED | NOT_A_DIRECTORY
        ));
    }
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn picker_listing_refuses_root_and_ancestor_symlink_replacements_in_git_and_walk() {
    for git_mode in [false, true] {
        for replaced_part in ["container", "container/project", "container/project/parent"] {
            let (_temp, root) = root();
            let (_outside, outside) = super::super::tests::root();
            write(
                &root,
                "container/project/parent/child/inside.txt",
                b"inside",
            );
            write(&outside, "project/parent/child/outside.txt", b"outside");
            write(&outside, "parent/child/outside.txt", b"outside");
            write(&outside, "child/outside.txt", b"outside");
            let project = root.join("container/project");
            if git_mode {
                git(&project, &["init", "-q"]);
            }
            let replaced = root.join(replaced_part);
            replace_after_resolve(replaced, outside);
            let error = list_files(project, "parent/child".into(), None)
                .await
                .unwrap_err();
            assert!(matches!(
                refusal_code(error).as_str(),
                write::IO_FAILED | NOT_A_DIRECTORY
            ));
        }
    }
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn directory_metadata_uses_pinned_handle_and_refuses_a_replaced_parent() {
    let (_temp, root) = root();
    let (_outside, outside) = super::super::tests::root();
    write(&root, "parent/inside.txt", b"in");
    write(&outside, "outside.txt", b"outside metadata");
    let parent = root.join("parent");
    BEFORE_METADATA
        .with(|hook| *hook.borrow_mut() = Some(Box::new(move || swap(&parent, &outside))));
    let error = list_directory_entries(&root, "parent", None).unwrap_err();
    assert_eq!(refusal_code(error), NOT_A_DIRECTORY);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn picker_walk_refuses_a_recursive_directory_swap() {
    let (_temp, root) = root();
    let (_outside, outside) = super::super::tests::root();
    write(&root, "parent/inside.txt", b"in");
    write(&outside, "outside.txt", b"outside");
    let error = tokio::task::spawn_blocking(move || {
        let parent = root.join("parent");
        BEFORE_DESCEND
            .with(|hook| *hook.borrow_mut() = Some(Box::new(move || swap(&parent, &outside))));
        let scope = DirectoryScope::resolve(&root, "").unwrap();
        let mut rules = IgnoreRules::parents(&scope).unwrap();
        walk(
            &scope.open().unwrap(),
            &root,
            &root,
            100,
            &mut rules,
            false,
            &mut Vec::new(),
            &mut false,
            0,
        )
        .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(refusal_code(error), write::IO_FAILED);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn listings_preserve_contained_directory_aliases_and_git_index_entries() {
    let (_temp, root) = root();
    write(&root, "real/a.txt", b"abc");
    std::os::unix::fs::symlink("real", root.join("alias")).unwrap();
    let entries = list_directory_entries(&root, "alias", None).unwrap();
    assert_eq!(entries.path, "alias");
    assert_eq!(entries.entries.len(), 1);
    assert_eq!(entries.entries[0].path, "alias/a.txt");
    assert_eq!(entries.entries[0].size, 3);
    let paths = list_files(root.clone(), "alias".into(), None)
        .await
        .unwrap();
    assert_eq!(paths.paths, ["real/a.txt"]);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "real/a.txt", "alias"]);
    std::fs::remove_file(root.join("real/a.txt")).unwrap();
    let paths = list_files(root, "".into(), None).await.unwrap();
    assert_eq!(paths.source, ProjectFilesSourceV1::Git);
    assert_eq!(paths.paths, ["alias", "real/a.txt"]);
}

#[cfg(not(unix))]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn listings_refuse_platforms_without_safe_directory_handles() {
    let (_temp, root) = root();
    assert_eq!(
        refusal_code(list_directory_entries(&root, "", None).unwrap_err()),
        write::UNSUPPORTED
    );
    assert_eq!(
        refusal_code(list_files(root, "".into(), None).await.unwrap_err()),
        write::UNSUPPORTED
    );
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn directory_listing_reports_link_metadata_after_a_leaf_swap() {
    let (_temp, root) = root();
    let (_outside, outside) = super::super::tests::root();
    write(&root, "inside.txt", b"in");
    write(&outside, "outside.txt", &[b'x'; 4096]);
    let leaf = root.join("inside.txt");
    let target = outside.join("outside.txt");
    BEFORE_METADATA.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::fs::remove_file(&leaf).unwrap();
            std::os::unix::fs::symlink(target, leaf).unwrap();
        }))
    });
    let listing = list_directory_entries(&root, "", None).unwrap();
    let metadata = std::fs::symlink_metadata(root.join("inside.txt")).unwrap();
    assert_eq!(listing.entries.len(), 1);
    let entry = &listing.entries[0];
    assert_eq!(entry.name, "inside.txt");
    assert_eq!(entry.kind, ProjectEntryKindV1::Symlink);
    assert_eq!(entry.size, metadata.len());
    assert_eq!(entry.mtime, Some(rfc3339(metadata.modified().unwrap())));
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn picker_walk_preserves_nested_ignore_precedence_and_requested_subtrees() {
    let (_temp, root) = root();
    write(&root, ".gitignore", b"*.log\n/sub/skip/\n");
    write(&root, ".ignore", b"*.tmp\n");
    write(&root, "sub/.gitignore", b"!keep.log\n");
    write(&root, "sub/.ignore", b"!keep.tmp\n");
    for path in [
        "sub/keep.log",
        "sub/drop.log",
        "sub/keep.tmp",
        "sub/drop.tmp",
        "sub/skip/out.txt",
        "sub/plain.txt",
    ] {
        write(&root, path, b"x");
    }
    let listing = list_files(root, "sub".into(), None).await.unwrap();
    assert_eq!(listing.source, ProjectFilesSourceV1::Walk);
    assert_eq!(
        listing.paths,
        [
            "sub/.gitignore",
            "sub/.ignore",
            "sub/keep.log",
            "sub/keep.tmp",
            "sub/plain.txt"
        ]
    );
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn git_picker_preserves_nested_repositories_and_ignored_tracked_files() {
    let (_temp, root) = root();
    git(&root, &["init", "-q"]);
    write(&root, "tracked.log", b"tracked");
    git(&root, &["add", "tracked.log"]);
    write(&root, ".gitignore", b"*.log\nbuild/\n");
    write(&root, ".ignore", b"keep.txt\n");
    write(&root, "keep.txt", b"keep");
    write(&root, "build/out.txt", b"skip");
    write(&root, "nested/inside.txt", b"nested");
    git(&root.join("nested"), &["init", "-q"]);
    let listing = list_files(root.clone(), "".into(), None).await.unwrap();
    assert_eq!(listing.source, ProjectFilesSourceV1::Git);
    assert_eq!(
        listing.paths,
        [
            ".gitignore",
            ".ignore",
            "keep.txt",
            "nested/",
            "tracked.log"
        ]
    );
    let nested = list_files(root, "nested".into(), None).await.unwrap();
    assert_eq!(nested.paths, ["nested/"]);
}
