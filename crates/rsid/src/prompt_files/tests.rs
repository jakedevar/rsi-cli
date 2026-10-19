//! hub #3040 / local #74: prompt-file containment, atomic writes and the
//! recoverable trash. Every test uses its own temp data directory.

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
use tempfile::TempDir;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(message) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn a_plain_name_round_trips_and_lists_newest_first() {
    let data = TempDir::new().unwrap();
    let empty = list(data.path()).unwrap();
    assert_eq!(empty.files, Vec::new());
    assert!(!empty.truncated);

    let created = put(data.path(), "first.md", "# First prompt\n\nbody\n", false).unwrap();
    assert!(created.created);
    assert_eq!(created.size, 21);
    let read = get(data.path(), "first.md").unwrap();
    assert_eq!(read.content, "# First prompt\n\nbody\n");
    assert_eq!(read.name, "first.md");

    let replaced = put(data.path(), "first.md", "# Edited\n", false).unwrap();
    assert!(!replaced.created);
    assert_eq!(get(data.path(), "first.md").unwrap().content, "# Edited\n");
    std::thread::sleep(std::time::Duration::from_millis(20));
    put(data.path(), "second.md", "## Second\n", false).unwrap();

    let listing = list(data.path()).unwrap();
    let names: Vec<_> = listing.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["second.md", "first.md"]);
    assert_eq!(listing.files[0].first_line, "Second");
    assert_eq!(listing.files[1].first_line, "Edited");
    assert_eq!(
        Path::new(&listing.dir),
        std::fs::canonicalize(data.path().join("prompts")).unwrap()
    );
    // The write left no temp file behind.
    let entries: Vec<_> = std::fs::read_dir(data.path().join("prompts"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries.len(), 2, "{entries:?}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn escaping_and_non_md_names_are_refused_by_every_method() {
    let data = TempDir::new().unwrap();
    std::fs::create_dir_all(data.path().join("prompts")).unwrap();
    std::fs::write(data.path().join("outside.md"), "secret").unwrap();
    let absolute = data.path().join("outside.md").display().to_string();
    for name in [
        "../outside.md",
        "..",
        "sub/a.md",
        "sub\\a.md",
        absolute.as_str(),
        "a.txt",
        "notes",
        "",
    ] {
        assert!(
            code(get(data.path(), name).unwrap_err()).starts_with("prompt_file_invalid_name"),
            "get {name:?}"
        );
        assert!(
            code(put(data.path(), name, "x", false).unwrap_err())
                .starts_with("prompt_file_invalid_name"),
            "put {name:?}"
        );
        assert!(
            code(delete(data.path(), name).unwrap_err()).starts_with("prompt_file_invalid_name"),
            "delete {name:?}"
        );
    }
    // Nothing outside the prompts directory was touched.
    assert_eq!(
        std::fs::read_to_string(data.path().join("outside.md")).unwrap(),
        "secret"
    );
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn a_symlinked_prompt_is_refused_and_never_written_through() {
    let data = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let secret = outside.path().join("secret.md");
    std::fs::write(&secret, "outside text").unwrap();
    std::fs::create_dir_all(data.path().join("prompts")).unwrap();
    std::os::unix::fs::symlink(&secret, data.path().join("prompts/link.md")).unwrap();

    assert!(code(get(data.path(), "link.md").unwrap_err()).starts_with("prompt_file_symlink"));
    assert!(
        code(put(data.path(), "link.md", "overwrite", false).unwrap_err())
            .starts_with("prompt_file_symlink")
    );
    assert!(code(delete(data.path(), "link.md").unwrap_err()).starts_with("prompt_file_symlink"));
    assert_eq!(list(data.path()).unwrap().files, Vec::new());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "outside text");
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn a_prompts_directory_symlinked_out_of_the_data_dir_is_refused() {
    let data = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("a.md"), "outside").unwrap();
    std::os::unix::fs::symlink(outside.path(), data.path().join("prompts")).unwrap();

    assert!(code(list(data.path()).unwrap_err()).starts_with("prompt_file_outside_root"));
    assert!(code(get(data.path(), "a.md").unwrap_err()).starts_with("prompt_file_outside_root"));
    assert!(
        code(put(data.path(), "b.md", "x", false).unwrap_err())
            .starts_with("prompt_file_outside_root")
    );
    assert!(!outside.path().join("b.md").exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn create_only_refuses_an_existing_name_and_sizes_are_capped() {
    let data = TempDir::new().unwrap();
    put(data.path(), "a.md", "one", true).unwrap();
    assert!(
        code(put(data.path(), "a.md", "two", true).unwrap_err()).starts_with("prompt_file_exists")
    );
    assert_eq!(get(data.path(), "a.md").unwrap().content, "one");

    let big = "x".repeat(MAX_PROMPT_FILE_BYTES + 1);
    assert!(
        code(put(data.path(), "big.md", &big, false).unwrap_err())
            .starts_with("prompt_file_too_large")
    );
    std::fs::write(data.path().join("prompts/big.md"), &big).unwrap();
    assert!(code(get(data.path(), "big.md").unwrap_err()).starts_with("prompt_file_too_large"));

    std::fs::write(data.path().join("prompts/bin.md"), [0xff, 0xfe, 0x00]).unwrap();
    assert!(code(get(data.path(), "bin.md").unwrap_err()).starts_with("prompt_file_not_utf8"));
    assert!(code(get(data.path(), "missing.md").unwrap_err()).starts_with("prompt_file_not_found"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn delete_moves_the_prompt_to_a_recoverable_trash_path() {
    let data = TempDir::new().unwrap();
    put(data.path(), "keep.md", "# Keep me\n", false).unwrap();
    let deleted = delete(data.path(), "keep.md").unwrap();
    let trash_path = PathBuf::from(&deleted.trash_path);

    assert_eq!(
        trash_path.parent().unwrap(),
        std::fs::canonicalize(data.path().join("trash/prompts")).unwrap()
    );
    let file_name = trash_path.file_name().unwrap().to_str().unwrap();
    assert!(file_name.ends_with("Z-keep.md"), "{file_name}");
    assert_eq!(std::fs::read_to_string(&trash_path).unwrap(), "# Keep me\n");
    assert!(code(get(data.path(), "keep.md").unwrap_err()).starts_with("prompt_file_not_found"));
    assert_eq!(list(data.path()).unwrap().files, Vec::new());

    // A second delete of the same name never clobbers the first trash entry.
    put(data.path(), "keep.md", "second", false).unwrap();
    let again = delete(data.path(), "keep.md").unwrap();
    assert_ne!(again.trash_path, deleted.trash_path);
    assert_eq!(std::fs::read_to_string(&trash_path).unwrap(), "# Keep me\n");

    // Recovery is a move back into the prompts directory.
    std::fs::rename(&trash_path, data.path().join("prompts/keep.md")).unwrap();
    assert_eq!(get(data.path(), "keep.md").unwrap().content, "# Keep me\n");
    assert!(code(delete(data.path(), "gone.md").unwrap_err()).starts_with("prompt_file_not_found"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn a_trash_entry_name_collision_picks_the_next_name() {
    let data = TempDir::new().unwrap();
    let dir = data.path().join("prompts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.md"), "new").unwrap();
    let trash = data.path().join("trash/prompts");
    std::fs::create_dir_all(&trash).unwrap();
    std::fs::write(trash.join("taken.md"), "old").unwrap();
    assert!(!move_no_clobber(&dir.join("a.md"), &trash.join("taken.md")).unwrap());
    assert_eq!(
        std::fs::read_to_string(trash.join("taken.md")).unwrap(),
        "old"
    );
    assert!(move_no_clobber(&dir.join("a.md"), &trash.join("free.md")).unwrap());
    assert!(!dir.join("a.md").exists());
}
