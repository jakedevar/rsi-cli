//! hub #3012: path scope, caps and each project-file read.

use super::*;
use std::process::Command;
use tempfile::TempDir;

/// A tempdir and its canonical root (macOS `/var` -> `/private/var`).
pub(super) fn root() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let canonical = canonical_root(dir.path()).unwrap();
    (dir, canonical)
}

pub(super) fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

pub(super) fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=fixture",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

pub(super) fn refusal_code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(message) => message.split(':').next().unwrap().to_string(),
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn parent_dir_component_is_refused_even_when_it_would_stay_inside() {
    let (_dir, root) = root();
    write(&root, "a/b.txt", b"x");
    for path in ["../etc/passwd", "a/../a/b.txt", "a/../../x"] {
        let error = resolve_in_root(&root, path).unwrap_err();
        assert_eq!(refusal_code(error), OUTSIDE_ROOT, "{path}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn absolute_path_is_refused_even_inside_the_root() {
    let (_dir, root) = root();
    write(&root, "a.txt", b"x");
    let inside = root.join("a.txt");
    for path in [
        inside.to_string_lossy().into_owned(),
        "/etc/passwd".to_string(),
    ] {
        let error = resolve_in_root(&root, &path).unwrap_err();
        assert_eq!(refusal_code(error), OUTSIDE_ROOT, "{path}");
    }
}

#[cfg(target_os = "windows")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn windows_drive_and_unc_paths_are_refused() {
    let (_dir, root) = root();
    for path in [r"C:\Windows", r"C:Windows", r"\\server\share\x", r"\x"] {
        let error = resolve_in_root(&root, path).unwrap_err();
        assert_eq!(refusal_code(error), OUTSIDE_ROOT, "{path}");
    }
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn symlink_resolving_outside_the_root_is_refused_and_inside_one_is_followed() {
    let (_outside_dir, outside) = root();
    write(&outside, "secret.txt", b"secret");
    let (_dir, root) = root();
    write(&root, "real/inner.txt", b"inner");
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("leak.txt")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();

    for path in ["escape", "escape/secret.txt", "leak.txt"] {
        let error = resolve_in_root(&root, path).unwrap_err();
        assert_eq!(refusal_code(error), OUTSIDE_ROOT, "{path}");
    }
    let error = read_file(&root, "leak.txt", None, None, None).unwrap_err();
    assert_eq!(refusal_code(error), OUTSIDE_ROOT);

    let scoped = resolve_in_root(&root, "alias/inner.txt").unwrap();
    assert_eq!(scoped.display, "alias/inner.txt");
    assert_eq!(scoped.real_relative, "real/inner.txt");
    assert_eq!(scoped.target, root.join("real/inner.txt"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn missing_path_is_not_found_and_a_non_canonical_root_resolves() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "x/y.txt", b"y");
    // The tempdir path itself is not canonical on macOS (/var -> /private/var).
    let root = canonical_root(dir.path()).unwrap();
    assert_eq!(root, dir.path().canonicalize().unwrap());
    let error = resolve_in_root(&root, "x/missing.txt").unwrap_err();
    assert_eq!(refusal_code(error), NOT_FOUND);
    let scoped = resolve_in_root(&root, "./x//y.txt").unwrap();
    assert_eq!(scoped.display, "x/y.txt");
    assert_eq!(resolve_in_root(&root, "").unwrap().target, root);
    assert_eq!(resolve_in_root(&root, ".").unwrap().display, "");

    let error = canonical_root(&dir.path().join("no-such-root")).unwrap_err();
    assert_eq!(refusal_code(error), UNKNOWN_ROOT);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn directory_listing_sorts_dirs_first_flags_hidden_and_caps_entries() {
    let (_dir, root) = root();
    write(&root, "b.txt", b"bb");
    write(&root, "A.txt", b"a");
    write(&root, ".env", b"secret=1");
    write(&root, "zdir/x", b"");
    write(&root, "adir/x", b"");

    let listing = list_directory_entries(&root, "", None).unwrap();
    let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["adir", "zdir", ".env", "A.txt", "b.txt"]);
    assert!(!listing.truncated);
    assert_eq!(listing.root, root.to_string_lossy());
    let env = &listing.entries[2];
    assert!(env.hidden);
    assert_eq!(env.kind, ProjectEntryKindV1::File);
    assert_eq!(env.size, 8);
    assert!(env.mtime.as_deref().unwrap().ends_with('Z'));
    assert_eq!(listing.entries[0].kind, ProjectEntryKindV1::Dir);
    assert!(!listing.entries[3].hidden);

    let capped = list_directory_entries(&root, "", Some(2)).unwrap();
    assert!(capped.truncated);
    assert_eq!(capped.entries.len(), 2);

    let nested = list_directory_entries(&root, "adir", None).unwrap();
    assert_eq!(nested.path, "adir");
    assert_eq!(nested.entries[0].path, "adir/x");

    let error = list_directory_entries(&root, "b.txt", None).unwrap_err();
    assert_eq!(refusal_code(error), NOT_A_DIRECTORY);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn directory_listing_reports_a_symlink_as_a_symlink() {
    let (_dir, root) = root();
    write(&root, "real.txt", b"r");
    std::os::unix::fs::symlink("/etc", root.join("outside")).unwrap();
    let listing = list_directory_entries(&root, "", None).unwrap();
    let link = listing
        .entries
        .iter()
        .find(|e| e.name == "outside")
        .unwrap();
    assert_eq!(link.kind, ProjectEntryKindV1::Symlink);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn directory_listing_flags_gitignored_entries_without_dropping_them() {
    let (_dir, root) = root();
    git(&root, &["init", "-q"]);
    write(&root, ".gitignore", b"target/\n*.log\n");
    write(&root, "target/debug/out", b"");
    write(&root, "run.log", b"");
    write(&root, "src/main.rs", b"fn main() {}\n");

    let listing = list_directory(root.clone(), String::new(), None)
        .await
        .unwrap();
    let flag = |name: &str| {
        listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} listed"))
            .ignored
    };
    assert!(flag("target"));
    assert!(flag("run.log"));
    assert!(!flag("src"));
    assert!(!flag(".gitignore"));

    let inner = list_directory(root.clone(), "target".into(), None)
        .await
        .unwrap();
    assert!(inner.entries.iter().all(|e| e.ignored), "{inner:?}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn picker_feed_lists_tracked_and_untracked_but_not_ignored_and_caps() {
    let (_dir, root) = root();
    git(&root, &["init", "-q"]);
    write(&root, ".gitignore", b"*.log\n");
    write(&root, "src/lib.rs", b"//\n");
    write(&root, "README.md", b"#\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    write(&root, "new.txt", b"n");
    write(&root, "debug.log", b"l");

    let feed = list_files(root.clone(), String::new(), None).await.unwrap();
    assert_eq!(feed.source, ProjectFilesSourceV1::Git);
    let mut paths = feed.paths.clone();
    paths.sort();
    assert_eq!(paths, [".gitignore", "README.md", "new.txt", "src/lib.rs"]);
    assert!(!feed.truncated);

    let capped = list_files(root.clone(), String::new(), Some(2))
        .await
        .unwrap();
    assert_eq!(capped.paths.len(), 2);
    assert!(capped.truncated);

    let scoped = list_files(root.clone(), "src".into(), None).await.unwrap();
    assert_eq!(scoped.paths, ["src/lib.rs"]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn picker_feed_walks_a_root_outside_git_honouring_gitignore() {
    let (_dir, root) = root();
    write(&root, ".gitignore", b"skip/\n");
    write(&root, "keep/a.txt", b"a");
    write(&root, "skip/b.txt", b"b");
    write(&root, ".hidden", b"h");
    let feed = list_files(root.clone(), String::new(), None).await.unwrap();
    assert_eq!(feed.source, ProjectFilesSourceV1::Walk);
    assert_eq!(feed.paths, [".gitignore", ".hidden", "keep/a.txt"]);
    let capped = list_files(root.clone(), String::new(), Some(1))
        .await
        .unwrap();
    assert!(capped.truncated);
    assert_eq!(capped.paths.len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn read_returns_text_and_enforces_the_byte_cap() {
    let (_dir, root) = root();
    write(&root, "a.txt", b"one\ntwo\nthree\n");
    let read = read_file(&root, "a.txt", None, None, None).unwrap();
    assert_eq!(read.content.as_deref(), Some("one\ntwo\nthree\n"));
    assert!(!read.binary);
    assert!(!read.truncated);
    assert_eq!(read.size, 14);
    assert_eq!((read.start_line, read.end_line), (Some(1), Some(3)));

    let capped = read_file(&root, "a.txt", Some(5), None, None).unwrap();
    assert_eq!(capped.content.as_deref(), Some("one\nt"));
    assert!(capped.truncated);
    assert_eq!(capped.size, 14);

    // A cap inside a multi-byte character drops the partial character.
    write(&root, "u.txt", "aé".as_bytes());
    let cut = read_file(&root, "u.txt", Some(2), None, None).unwrap();
    assert_eq!(cut.content.as_deref(), Some("a"));
    assert!(cut.truncated);

    let error = read_file(&root, "", None, None, None).unwrap_err();
    assert_eq!(refusal_code(error), NOT_A_FILE);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn read_reports_binary_with_no_content() {
    let (_dir, root) = root();
    write(&root, "nul.bin", b"PK\x03\x04\x00\x00data");
    write(&root, "latin1.txt", b"caf\xe9\n");
    for path in ["nul.bin", "latin1.txt"] {
        let read = read_file(&root, path, None, None, None).unwrap();
        assert!(read.binary, "{path}");
        assert_eq!(read.content, None, "{path}");
    }
    let ranged = read_file(&root, "nul.bin", None, Some(1), Some(1)).unwrap();
    assert!(ranged.binary);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn read_line_range_is_inclusive_and_capped_at_line_boundaries() {
    let (_dir, root) = root();
    write(&root, "l.txt", b"1\n22\n333\n4444\n");
    let mid = read_file(&root, "l.txt", None, Some(2), Some(3)).unwrap();
    assert_eq!(mid.content.as_deref(), Some("22\n333\n"));
    assert_eq!((mid.start_line, mid.end_line), (Some(2), Some(3)));
    assert!(!mid.truncated);

    let tail = read_file(&root, "l.txt", None, Some(4), None).unwrap();
    assert_eq!(tail.content.as_deref(), Some("4444\n"));

    let capped = read_file(&root, "l.txt", Some(6), Some(2), None).unwrap();
    assert_eq!(capped.content.as_deref(), Some("22\n"));
    assert_eq!(capped.end_line, Some(2));
    assert!(capped.truncated);

    let long_first = read_file(&root, "l.txt", Some(2), Some(4), None).unwrap();
    assert_eq!(long_first.content.as_deref(), Some("44"));
    assert!(long_first.truncated);

    let past_end = read_file(&root, "l.txt", None, Some(9), None).unwrap();
    assert_eq!(past_end.content.as_deref(), Some(""));
    assert_eq!(past_end.start_line, None);

    let error = read_file(&root, "l.txt", None, Some(3), Some(2)).unwrap_err();
    assert_eq!(refusal_code(error), INVALID_REQUEST);
    let error = read_file(&root, "l.txt", None, Some(0), None).unwrap_err();
    assert_eq!(refusal_code(error), INVALID_REQUEST);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn unified_zero_hunks_become_gutter_markers() {
    use GitGutterMarkerKindV1::{Added, Modified, Removed};
    let diff = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -0,0 +1,2 @@
+a
+b
@@ -5 +7 @@ fn x()
-old
+new
@@ -9,2 +10,0 @@
-gone
-gone
@@ -20,1 +19,3 @@
-x
+y
+z
+w
@@ -30,3 +31,1 @@
-p
-q
-r
+s
";
    let markers: Vec<_> = parse_unified_zero(diff)
        .into_iter()
        .map(|m| (m.line, m.kind))
        .collect();
    assert_eq!(
        markers,
        [
            (1, Added),
            (2, Added),
            (7, Modified),
            (10, Removed),
            (19, Modified),
            (20, Added),
            (21, Added),
            (31, Modified),
            (31, Removed),
        ]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn git_gutter_marks_worktree_changes_against_head() {
    use GitGutterMarkerKindV1::{Added, Modified, Removed};
    let (_dir, root) = root();
    git(&root, &["init", "-q"]);
    write(&root, ".gitignore", b"*.log\n");
    write(&root, "f.txt", b"a\nb\nc\nd\n");
    write(&root, "same.txt", b"s\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);

    write(&root, "f.txt", b"A\nb\nd\nnew\n");
    let gutter = git_gutter(root.clone(), "f.txt".into()).await.unwrap();
    assert_eq!(gutter.status, GitGutterStatusV1::Tracked);
    let markers: Vec<_> = gutter.markers.iter().map(|m| (m.line, m.kind)).collect();
    assert_eq!(markers, [(1, Modified), (2, Removed), (4, Added)]);

    let same = git_gutter(root.clone(), "same.txt".into()).await.unwrap();
    assert_eq!(same.status, GitGutterStatusV1::Tracked);
    assert!(same.markers.is_empty());

    write(&root, "fresh.txt", b"x\ny\n");
    let fresh = git_gutter(root.clone(), "fresh.txt".into()).await.unwrap();
    assert_eq!(fresh.status, GitGutterStatusV1::Untracked);
    assert_eq!(
        fresh
            .markers
            .iter()
            .map(|m| (m.line, m.kind))
            .collect::<Vec<_>>(),
        [(1, Added), (2, Added)]
    );

    write(&root, "x.log", b"l\n");
    let ignored = git_gutter(root.clone(), "x.log".into()).await.unwrap();
    assert_eq!(ignored.status, GitGutterStatusV1::Ignored);

    let error = git_gutter(root.clone(), "../f.txt".into())
        .await
        .unwrap_err();
    assert_eq!(refusal_code(error), OUTSIDE_ROOT);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn git_gutter_outside_a_repo_says_so() {
    let (_dir, root) = root();
    write(&root, "f.txt", b"a\n");
    let gutter = git_gutter(root.clone(), "f.txt".into()).await.unwrap();
    assert_eq!(gutter.status, GitGutterStatusV1::NotARepo);
    assert!(gutter.markers.is_empty());
}
