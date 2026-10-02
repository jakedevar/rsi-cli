//! #389: manager evidence reads use local objects only. A partial clone whose
//! promised object is missing must not reach a remote, transport or credential
//! helper, and a timed-out read leaves no child process behind.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn sh(root: &Path, args: &[&str]) -> String {
    command(root, args)
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn invocations(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |text| text.lines().count())
}

/// A blobless partial clone whose only blob is missing locally, with an
/// instrumented upload-pack transport and credential helper on its remote.
struct Promisor {
    _dir: TempDir,
    clone: PathBuf,
    blob: String,
    commit: String,
    log: PathBuf,
}

fn promisor() -> Promisor {
    let dir = tempfile::tempdir_in(evidence_temp_root()).unwrap();
    let origin = dir.path().join("origin");
    let clone = dir.path().join("clone");
    let log = dir.path().join("helper.log");
    std::fs::create_dir(&origin).unwrap();
    sh(&origin, &["init", "-q", "-b", "main"]);
    sh(&origin, &["config", "user.email", "t@example.com"]);
    sh(&origin, &["config", "user.name", "t"]);
    sh(&origin, &["config", "uploadpack.allowFilter", "true"]);
    sh(
        &origin,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );
    std::fs::write(origin.join("a.txt"), "promised content\n").unwrap();
    sh(&origin, &["add", "a.txt"]);
    sh(&origin, &["commit", "-q", "-m", "one"]);
    let blob = sh(&origin, &["rev-parse", "HEAD:a.txt"]).trim().to_string();
    let commit = sh(&origin, &["rev-parse", "HEAD"]).trim().to_string();
    let url = format!("file://{}", origin.display());
    sh(
        dir.path(),
        &[
            "clone",
            "-q",
            "--no-checkout",
            "--filter=blob:none",
            &url,
            "clone",
        ],
    );
    let upload = dir.path().join("upload-pack.sh");
    script(
        &upload,
        &format!(
            "echo upload-pack >> {}\nexec git-upload-pack \"$@\"",
            log.display()
        ),
    );
    let helper = dir.path().join("credential.sh");
    script(&helper, &format!("echo credential >> {}", log.display()));
    sh(
        &clone,
        &[
            "config",
            "remote.origin.uploadpack",
            &upload.display().to_string(),
        ],
    );
    sh(
        &clone,
        &[
            "config",
            "credential.helper",
            &format!("!{}", helper.display()),
        ],
    );
    Promisor {
        _dir: dir,
        clone,
        blob,
        commit,
        log,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_missing_promised_object_is_a_typed_error_and_never_reaches_a_transport() {
    let f = promisor();
    let object = format!("{}:a.txt", f.commit);
    for args in [
        vec!["cat-file", "blob", f.blob.as_str()],
        vec!["show", object.as_str()],
    ] {
        let error = git::read(&f.clone, &args).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manager_v2_git_missing_local_object"),
            "{args:?}: {error}"
        );
    }
    let error = git::content_blob(&f.clone, &f.commit, "a.txt")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_git_missing_local_object"),
        "{error}"
    );
    assert_eq!(
        invocations(&f.log),
        0,
        "no transport or credential helper ran"
    );
    // Objects that are local still read, and an absent path is still `None`.
    assert_eq!(
        String::from_utf8(
            git::read(&f.clone, &["cat-file", "commit", &f.commit])
                .await
                .unwrap()
        )
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .len(),
        45
    );
    assert_eq!(
        git::content_blob(&f.clone, &f.commit, "absent.txt")
            .await
            .unwrap(),
        None
    );
    assert_eq!(invocations(&f.log), 0);

    // The fixture is live: an unguarded read of the same object does fetch.
    let control = Command::new("git")
        .arg("-C")
        .arg(&f.clone)
        .args(["cat-file", "blob", &f.blob])
        .env("GIT_CEILING_DIRECTORIES", "/var/tmp")
        .output()
        .unwrap();
    assert!(invocations(&f.log) >= 1, "control run: {control:?}");
}

/// #389: an ancestry check against a commit that is not local is a typed
/// missing-object error, while exit 1 stays "not an ancestor".
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_missing_commit_in_an_ancestry_check_is_a_typed_error() {
    let f = promisor();
    let absent = "1".repeat(40);
    for (source, target) in [(&absent, &f.commit), (&f.commit, &absent)] {
        let error = git::ancestor(&f.clone, source, target).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manager_v2_git_missing_local_object"),
            "{source} {target}: {error}"
        );
    }
    // Real answers are unchanged: a commit is its own ancestor, and an
    // unrelated local commit is "not an ancestor" (exit 1), not an error.
    assert!(git::ancestor(&f.clone, &f.commit, &f.commit).await.unwrap());
    let other = sh(
        &f.clone,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit-tree",
            "-m",
            "unrelated",
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        ],
    )
    .trim()
    .to_string();
    assert!(!git::ancestor(&f.clone, &other, &f.commit).await.unwrap());
    assert_eq!(invocations(&f.log), 0, "no transport ran");
}

fn process_alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim().to_string())
        })
        .is_some_and(|rest| !rest.starts_with('Z'))
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_timed_out_evidence_read_leaves_no_child_process() {
    let dir = tempfile::tempdir_in(evidence_temp_root()).unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    sh(&root, &["init", "-q", "-b", "main"]);
    let pidfile = dir.path().join("sleep.pid");
    // A repo-local alias runs a shell that starts a long-lived grandchild.
    sh(
        &root,
        &[
            "config",
            "alias.hang",
            &format!("!sleep 300 & echo $! > {}; wait", pidfile.display()),
        ],
    );
    let started = std::time::Instant::now();
    let error = git::read_within(&root, &["hang"], std::time::Duration::from_millis(1500))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_git_timeout"),
        "{error}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("the alias ran and recorded its child")
        .trim()
        .parse()
        .unwrap();
    for _ in 0..50 {
        if !process_alive(pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("descendant {pid} survived the timeout");
}
