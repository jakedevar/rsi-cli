//! Case map for `release-guard` (#465): the Rust port of the inline interpreter
//! heredocs of `scripts/release-target-guard.sh`. The shell cases that drive them
//! through the installer are in `crates/rsi-repo-guards/tests/install_release_provenance.rs` (hub port) and `scripts/tests/test_install_release_links.sh`.
//!
//! * `lock_*`: the held `flock` (free lock taken; contention waits and says so;
//!   negative: a bad descriptor is refused, and a held lock really excludes).
//! * `links_into_*`: which symlinks lie in a release directory (sorted, de-duplicated,
//!   dangling links included; negative: links elsewhere and regular files are not listed).
//! * `kept_*`: preserve repoints a link at a hashed copy and records it; prune keeps
//!   exactly the copies a live (scanned or recorded) link resolves to and removes stale
//!   `.keep-*` files; negative: a link outside the release directory is untouched, and a
//!   link recorded in another clone's directory still protects its copy.
//! * `receipt_target_*`: (run against the hub's `release-target` guard, #3715, which replaced this crate's own `receipt-target`) the canonical target a receipt may deploy from (bare and wrapped
//!   `target_dir`, symlinks resolved, override agreement) and every refusal (relative,
//!   non-string, newline, not allowed, non-canonical override, differing override,
//!   unreadable receipt; exit 2 when it names none).
//! * `receipt_status_*`: every classification of a build job receipt.

use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::{kept, links_into, lock_fd, main, realpath, receipt_status};
use crate::Ctx;
use crate::testutil;

fn root() -> (tempfile::TempDir, PathBuf) {
    let dir = testutil::short_tempdir();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    (dir, canonical)
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn lock_takes_a_free_lock_and_a_held_one_excludes() {
    let (_dir, root) = root();
    let path = root.join("lock");
    let first = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    let second = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let mut err = Vec::new();
    assert_eq!(lock_fd(&first.as_raw_fd().to_string(), &mut err), 0);
    assert!(err.is_empty());
    // SAFETY: a non-blocking flock probe on a descriptor this test owns.
    let busy = unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(busy, -1, "the lock must exclude a second holder");
}

#[test]
fn lock_waits_for_another_holder_and_says_so() {
    let (_dir, root) = root();
    let path = root.join("lock");
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    let waiter = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    // SAFETY: a blocking flock on a descriptor this test owns.
    assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(holder);
    });
    let started = std::time::Instant::now();
    let mut err = Vec::new();
    assert_eq!(lock_fd(&waiter.as_raw_fd().to_string(), &mut err), 0);
    assert!(started.elapsed() >= std::time::Duration::from_millis(200));
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("Waiting for another release build into this target"),
    );
    release.join().unwrap();
}

#[test]
fn lock_refuses_a_bad_descriptor() {
    let mut err = Vec::new();
    assert_eq!(lock_fd("not-a-number", &mut err), 64);
    assert_eq!(lock_fd("987654", &mut err), 1);
}

#[test]
fn links_into_lists_only_links_that_resolve_into_the_release_dir() {
    let (_dir, root) = root();
    let release = root.join("out/release");
    write(&release.join("rsid"), "r");
    write(&root.join("elsewhere/other"), "o");
    let bin = root.join("bin");
    let wt = root.join("wt/target/release");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    symlink(release.join("rsid"), bin.join("rsid")).unwrap();
    symlink(release.join("gone"), bin.join("dangling")).unwrap(); // dangling, still into release
    symlink(root.join("elsewhere/other"), bin.join("other")).unwrap();
    write(&bin.join("plain"), "not a link");
    symlink("../../../out/release/rsid", wt.join("rsi")).unwrap(); // relative link
    // `out/releasefoo` shares the prefix string but is not the release directory.
    write(&root.join("out/releasefoo/x"), "x");
    symlink(root.join("out/releasefoo/x"), bin.join("lookalike")).unwrap();

    let mut out = Vec::new();
    let dirs = [s(&bin), s(&wt), s(&bin), s(&root.join("missing"))];
    let dir_refs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    links_into(&s(&release), &dir_refs, &mut out);
    let listed: Vec<String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        listed,
        [
            s(&bin.join("dangling")),
            s(&bin.join("rsid")),
            s(&wt.join("rsi"))
        ],
        "sorted by name within each directory, directories de-duplicated"
    );
}

fn run_kept(mode: &str, release: &Path, keep: &Path, dirs: &[&Path]) -> (i32, String) {
    let mut err = Vec::new();
    let dir_strings: Vec<String> = dirs.iter().map(|d| s(d)).collect();
    let dir_refs: Vec<&str> = dir_strings.iter().map(String::as_str).collect();
    let code = kept(mode, &s(release), &s(keep), &dir_refs, &mut err);
    (code, String::from_utf8(err).unwrap())
}

fn names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn kept_preserve_repoints_a_link_at_a_hashed_copy_and_records_it() {
    let (_dir, root) = root();
    let release = root.join("target/release");
    write(&release.join("rsid"), "OLD-rsid");
    let bin = root.join("bin");
    let keep = root.join("keep");
    std::fs::create_dir_all(&bin).unwrap();
    symlink(release.join("rsid"), bin.join("rsid")).unwrap();
    write(&root.join("elsewhere/other"), "untouched");
    symlink(root.join("elsewhere/other"), bin.join("other")).unwrap();

    let (code, err) = run_kept("preserve", &release, &keep, &[&bin]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("Kept installed link"), "{err}");
    let target = std::fs::read_link(bin.join("rsid")).unwrap();
    assert_eq!(target.parent().unwrap(), keep);
    let file = target.file_name().unwrap().to_string_lossy().into_owned();
    assert!(file.starts_with("rsid."), "{file}");
    assert_eq!(file.len(), "rsid.".len() + 16, "{file}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "OLD-rsid");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o755
    );
    // The build then overwrites the release file; the installed link keeps the old bytes.
    std::fs::write(release.join("rsid"), "NEW-rsid").unwrap();
    assert_eq!(
        std::fs::read_to_string(bin.join("rsid")).unwrap(),
        "OLD-rsid"
    );
    // Negative: a link that never pointed into the release directory is left alone.
    assert_eq!(
        std::fs::read_link(bin.join("other")).unwrap(),
        root.join("elsewhere/other")
    );
    let manifest = std::fs::read_to_string(keep.join(".links")).unwrap();
    assert_eq!(manifest, format!("{}\n", bin.join("rsid").display()));

    // Idempotent: the link now resolves outside the release directory.
    let (code, err) = run_kept("preserve", &release, &keep, &[&bin]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("Kept installed link"), "{err}");
    assert_eq!(std::fs::read_link(bin.join("rsid")).unwrap(), target);
}

#[test]
fn kept_prune_keeps_exactly_the_live_copies() {
    let (_dir, root) = root();
    let keep = root.join("keep");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write(&keep.join("rsid.aaaaaaaaaaaaaaaa"), "live");
    write(&keep.join("rsi.bbbbbbbbbbbbbbbb"), "orphan");
    write(&keep.join(".keep-9999-1"), "a copy a run died writing");
    symlink(keep.join("rsid.aaaaaaaaaaaaaaaa"), bin.join("rsid")).unwrap();
    // Another clone's worktree link (not scanned this run) protects its copy through `.links`.
    let other = root.join("other-clone");
    std::fs::create_dir_all(&other).unwrap();
    write(&keep.join("rsi-rpc.cccccccccccccccc"), "recorded");
    symlink(keep.join("rsi-rpc.cccccccccccccccc"), other.join("rsi-rpc")).unwrap();
    write(
        &keep.join(".links"),
        &format!("{}\n", other.join("rsi-rpc").display()),
    );

    let (code, err) = run_kept("prune", &root.join("target/release"), &keep, &[&bin]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        names(&keep),
        BTreeSet::from([
            ".lock".to_string(),
            ".links".to_string(),
            "rsid.aaaaaaaaaaaaaaaa".to_string(),
            "rsi-rpc.cccccccccccccccc".to_string(),
        ])
    );
    assert!(err.contains("Pruned kept copy"), "{err}");
    assert!(err.contains("rsi.bbbbbbbbbbbbbbbb"), "{err}");
    assert!(
        !err.contains(".keep-"),
        "stale writer copies are removed silently: {err}"
    );
    let manifest: BTreeSet<String> = std::fs::read_to_string(keep.join(".links"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        manifest,
        BTreeSet::from([s(&bin.join("rsid")), s(&other.join("rsi-rpc"))])
    );
    // Once the recorded link is gone, its copy is pruned too.
    std::fs::remove_file(other.join("rsi-rpc")).unwrap();
    let (code, _) = run_kept("prune", &root.join("target/release"), &keep, &[&bin]);
    assert_eq!(code, 0);
    assert!(!keep.join("rsi-rpc.cccccccccccccccc").exists());
}

#[test]
fn kept_refuses_an_unknown_mode() {
    let (_dir, root) = root();
    let (code, err) = run_kept("clobber", &root, &root.join("keep"), &[]);
    assert_eq!(code, 64);
    assert!(err.contains("preserve or prune"), "{err}");
}

fn receipt_in(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    s(&path)
}

fn target(receipt: &str, override_dir: &str, allowed: &[&Path]) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let allowed: Vec<String> = allowed.iter().map(|p| s(p)).collect();
    let mut args = vec![receipt.to_string(), override_dir.to_string()];
    args.extend(allowed);
    let ctx = Ctx::new(Path::new("/"), &[]);
    let code = crate::release_target::main(&args, &ctx, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn receipt_target_accepts_an_allowed_directory_bare_wrapped_or_through_a_symlink() {
    let (_dir, root) = root();
    let real = root.join("real-target");
    std::fs::create_dir_all(&real).unwrap();
    let alias = root.join("alias");
    symlink(&real, &alias).unwrap();
    for body in [
        format!(r#"{{"target_dir": "{}"}}"#, real.display()),
        format!(r#"{{"result": {{"target_dir": "{}"}}}}"#, real.display()),
        format!(r#"{{"target_dir": "{}"}}"#, alias.display()),
    ] {
        let receipt = receipt_in(&root, "receipt.json", &body);
        let (code, out, err) = target(&receipt, "", &[&real]);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (0, format!("{}\n", real.display()).as_str(), ""),
            "{body}"
        );
    }
}

#[test]
fn receipt_target_refuses_what_it_must() {
    let (_dir, root) = root();
    let real = root.join("real-target");
    let other = root.join("other-target");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let cases: [(&str, String, &str); 7] = [
        (
            "not allowed",
            format!(r#"{{"target_dir": "{}"}}"#, other.display()),
            "not an allowed deploy target",
        ),
        (
            "relative",
            r#"{"target_dir": "target"}"#.to_string(),
            "must be an absolute path",
        ),
        (
            "a number",
            r#"{"target_dir": 5}"#.to_string(),
            "non-empty absolute path string, got 5",
        ),
        (
            "empty",
            r#"{"target_dir": ""}"#.to_string(),
            "non-empty absolute path string, got ''",
        ),
        (
            "a newline",
            r#"{"target_dir": "/a\nb"}"#.to_string(),
            "must not contain a newline",
        ),
        (
            "a list",
            r#"{"target_dir": ["/a"]}"#.to_string(),
            "got [\"/a\"]",
        ),
        (
            "garbage",
            "not json".to_string(),
            "Cannot read build receipt",
        ),
    ];
    for (what, body, message) in cases {
        let receipt = receipt_in(&root, "receipt.json", &body);
        let (code, out, err) = target(&receipt, "", &[&real]);
        assert_eq!(code, 1, "{what}: {err}");
        assert!(out.is_empty(), "{what}: {out}");
        assert!(err.contains(message), "{what}: {err}");
    }
    let (code, out, err) = target(&s(&root.join("absent.json")), "", &[&real]);
    assert_eq!((code, out.as_str()), (1, ""), "{err}");
}

#[test]
fn receipt_target_without_a_target_dir_needs_a_canonical_override() {
    let (_dir, root) = root();
    let real = root.join("real-target");
    std::fs::create_dir_all(&real).unwrap();
    let receipt = receipt_in(&root, "receipt.json", r#"{"result": {"exit_code": 0}}"#);
    let (code, out, err) = target(&receipt, "", &[&real]);
    assert_eq!((code, out.as_str(), err.as_str()), (2, "", ""));
    let (code, out, _) = target(&receipt, &s(&real), &[&real]);
    assert_eq!((code, out), (0, format!("{}\n", real.display())));
    // The override never widens the allowed set (hub #3715): an unlisted one is refused.
    let (code, out, err) = target(&receipt, &s(&real), &[]);
    assert_eq!((code, out.as_str()), (1, ""), "{err}");
    assert!(err.contains("not an allowed deploy target"), "{err}");
    // Negative: the override must be canonical, absolute, an existing directory.
    let alias = root.join("alias");
    symlink(&real, &alias).unwrap();
    let dotted = format!("{}/../real-target", real.display());
    for (what, bad, message) in [
        ("symlink", s(&alias), "must be canonical"),
        ("dot-dot", dotted, "must be canonical"),
        ("relative", "target".to_string(), "must be an absolute path"),
        ("missing", s(&root.join("nope")), "is not a directory"),
    ] {
        let (code, out, err) = target(&receipt, &bad, &[&real]);
        assert_eq!(code, 1, "{what}: {err}");
        assert!(out.is_empty());
        assert!(err.contains(message), "{what}: {err}");
    }
}

#[test]
fn receipt_target_override_must_agree_with_the_receipt() {
    let (_dir, root) = root();
    let real = root.join("real-target");
    let other = root.join("other-target");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let receipt = receipt_in(
        &root,
        "receipt.json",
        &format!(r#"{{"target_dir": "{}"}}"#, real.display()),
    );
    // Agreeing, and itself allowed (hub #3715).
    let (code, out, _) = target(&receipt, &s(&real), &[&real]);
    assert_eq!((code, out), (0, format!("{}\n", real.display())));
    let (code, out, err) = target(&receipt, &s(&other), &[&real, &other]);
    assert_eq!(code, 1);
    assert!(out.is_empty());
    assert!(err.contains("differs from --target-dir"), "{err}");
}

fn status(body: &str) -> String {
    let (_dir, root) = root();
    let receipt = receipt_in(&root, "r.json", body);
    let mut out = Vec::new();
    assert_eq!(receipt_status(&receipt, &mut out), 0);
    String::from_utf8(out).unwrap()
}

#[test]
fn receipt_status_classifies_a_build_receipt() {
    let sha = "0123456789012345678901234567890123456789";
    assert_eq!(
        status(&format!(
            r#"{{"exit_code": 0, "source_head": "{sha}", "source_dirty": false}}"#
        )),
        format!("ok|{sha}|\n")
    );
    // Wrapped under `result`.
    assert_eq!(
        status(&format!(
            r#"{{"result": {{"exit_code": 0, "source_head": "{sha}", "source_dirty": false}}}}"#
        )),
        format!("ok|{sha}|\n")
    );
    // Negative: dirty, unrecorded dirtiness, no head, failure and refusal are never ok.
    assert_eq!(
        status(&format!(
            r#"{{"exit_code": 0, "source_head": "{sha}", "source_dirty": true}}"#
        )),
        format!("dirty|{sha}|\n")
    );
    assert_eq!(
        status(&format!(r#"{{"exit_code": 0, "source_head": "{sha}"}}"#)),
        format!("dirty|{sha}|\n")
    );
    assert_eq!(
        status(r#"{"exit_code": 0, "source_head": "", "source_dirty": false}"#),
        "nohead||\n"
    );
    assert_eq!(status(r#"{"exit_code": 0}"#), "nohead||\n");
    assert_eq!(
        status(r#"{"exit_code": 101, "source_head": "abc"}"#),
        "failed||exit_code 101\n"
    );
    assert_eq!(
        status(r#"{"source_head": "abc"}"#),
        "failed||exit_code None\n"
    );
    assert_eq!(
        status(r#"{"refusal": "build_slice_busy", "exit_code": 0, "source_head": "abc"}"#),
        "refused||build_slice_busy\n"
    );
    assert!(status("not json").starts_with("unreadable||"));
    assert_eq!(status("[1]"), "failed||exit_code None\n");
}

#[test]
fn receipt_status_keeps_the_detail_of_an_unreadable_receipt_on_one_line() {
    let line = status("{\n\"a\": \t}");
    assert!(line.starts_with("unreadable||"), "{line}");
    assert_eq!(line.lines().count(), 1, "{line}");
    assert!(!line.contains('\t'), "{line}");
}

#[test]
fn realpath_resolves_links_and_tolerates_missing_tails() {
    let (_dir, root) = root();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    symlink(root.join("a/b"), root.join("link")).unwrap();
    symlink("a/b", root.join("rel")).unwrap();
    symlink(root.join("a/nothing"), root.join("dangling")).unwrap();
    assert_eq!(realpath(&root.join("link/x/../y")), root.join("a/b/y"));
    assert_eq!(realpath(&root.join("rel")), root.join("a/b"));
    assert_eq!(realpath(&root.join("dangling")), root.join("a/nothing"));
    symlink("loop2", root.join("loop1")).unwrap();
    symlink("loop1", root.join("loop2")).unwrap();
    let _ = realpath(&root.join("loop1")); // terminates
}

#[test]
fn main_dispatch_and_usage() {
    let ctx = Ctx::from_process();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(main(&[], &ctx, &mut out, &mut err), 64);
    assert_eq!(
        main(&["receipt-status".to_string()], &ctx, &mut out, &mut err),
        64
    );
    assert_eq!(
        main(
            &["bogus".to_string(), "x".to_string()],
            &ctx,
            &mut out,
            &mut err
        ),
        64
    );
    assert!(String::from_utf8(err).unwrap().contains("usage:"));
}
