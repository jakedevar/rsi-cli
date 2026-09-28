use super::*;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Repos {
    dir: tempfile::TempDir,
    local: PathBuf,
    remote: PathBuf,
    source: String,
    base: String,
}

impl Repos {
    fn local(&self) -> &Path {
        &self.local
    }
    fn remote(&self) -> &Path {
        &self.remote
    }
    fn publish(&self, sha: &str) {
        git(
            self.local(),
            &["push", "origin", &format!("{sha}:refs/heads/rolling")],
        );
    }
}

fn repos() -> Repos {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("local");
    let remote = dir.path().join("origin.git");
    std::fs::create_dir(&local).unwrap();
    git(&local, &["init", "-b", "rolling"]);
    git(&local, &["config", "user.name", "Test"]);
    git(&local, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(local.join("base"), "base").unwrap();
    git(&local, &["add", "base"]);
    git(&local, &["commit", "-m", "base"]);
    let base = git(&local, &["rev-parse", "HEAD"]);
    git(&local, &["switch", "-c", "source"]);
    std::fs::write(local.join("source"), "source").unwrap();
    git(&local, &["add", "source"]);
    git(&local, &["commit", "-m", "source"]);
    let source = git(&local, &["rev-parse", "HEAD"]);
    git(&local, &["switch", "rolling"]);
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare"]);
    git(
        &local,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    Repos {
        dir,
        local,
        remote,
        source,
        base,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_rejects_forward_revert_despite_source_ancestry() {
    let r = repos();
    r.publish(&r.source);
    git(r.local(), &["switch", "source"]);
    git(r.local(), &["revert", "--no-edit", &r.source]);
    let reverted = git(r.local(), &["rev-parse", "HEAD"]);
    r.publish(&reverted);
    assert!(
        git::ancestor(r.local(), &r.source, &reverted)
            .await
            .unwrap()
    );
    let error = git::accepted_content(r.local(), &r.base, &r.source, &reverted)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_lost")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_allows_independent_change_and_rejects_partial_revert() {
    let r = repos();
    git(r.local(), &["switch", "source"]);
    std::fs::write(r.local().join("source"), "alpha\nbeta\n").unwrap();
    git(r.local(), &["add", "source"]);
    git(r.local(), &["commit", "-m", "accepted lines"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);
    std::fs::write(r.local().join("base"), "independent\n").unwrap();
    git(r.local(), &["add", "base"]);
    git(r.local(), &["commit", "-m", "independent change"]);
    let evolved = git(r.local(), &["rev-parse", "HEAD"]);
    git::accepted_content(r.local(), &r.source, &accepted, &evolved)
        .await
        .unwrap();
    std::fs::write(r.local().join("source"), "alpha\n").unwrap();
    git(r.local(), &["add", "source"]);
    git(r.local(), &["commit", "-m", "drop accepted beta"]);
    let partial = git(r.local(), &["rev-parse", "HEAD"]);
    let error = git::accepted_content(r.local(), &r.source, &accepted, &partial)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_lost")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_rejects_restored_deletion() {
    let r = repos();
    git(r.local(), &["switch", "source"]);
    git(r.local(), &["rm", "base"]);
    git(r.local(), &["commit", "-m", "delete base"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);
    git(r.local(), &["restore", "--source", &r.source, "--", "base"]);
    git(r.local(), &["add", "base"]);
    git(r.local(), &["commit", "-m", "restore base"]);
    let restored = git(r.local(), &["rev-parse", "HEAD"]);
    let error = git::accepted_content(r.local(), &r.source, &accepted, &restored)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_lost")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_skips_oversized_base_blob_deleted_on_both_sides() {
    let r = repos();
    git(r.local(), &["switch", "source"]);
    let wal = r.local().join(".fractal/fractal.db-wal");
    std::fs::create_dir_all(wal.parent().unwrap()).unwrap();
    std::fs::write(&wal, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
    git(r.local(), &["add", ".fractal/fractal.db-wal"]);
    git(r.local(), &["commit", "-m", "old WAL base"]);
    let base = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["rm", ".fractal/fractal.db-wal"]);
    std::fs::write(r.local().join("source"), "accepted change\n").unwrap();
    git(
        r.local(),
        &["commit", "-am", "delete WAL and change source"],
    );
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);
    std::fs::write(r.local().join("target-only"), "independent\n").unwrap();
    git(r.local(), &["add", "target-only"]);
    git(r.local(), &["commit", "-m", "independent target change"]);
    let target = git(r.local(), &["rev-parse", "HEAD"]);
    git::accepted_content(r.local(), &base, &accepted, &target)
        .await
        .unwrap();

    std::fs::write(r.local().join("source"), "source").unwrap();
    git(r.local(), &["commit", "-am", "lose accepted change"]);
    let lost = git(r.local(), &["rev-parse", "HEAD"]);
    let error = git::accepted_content(r.local(), &base, &accepted, &lost)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_lost")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_allows_merged_rolling_and_same_file_evolution() {
    let r = repos();
    std::fs::write(r.local().join("shared"), "top\nmiddle\nbottom\n").unwrap();
    git(r.local(), &["add", "shared"]);
    git(r.local(), &["commit", "-m", "rolling shared content"]);
    let rolling_parent = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "source"]);
    git(r.local(), &["merge", "--no-edit", &rolling_parent]);
    std::fs::write(
        r.local().join("shared"),
        "top\nmiddle\nbottom\nsource addition\n",
    )
    .unwrap();
    git(r.local(), &["commit", "-am", "source shared addition"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);
    let source_edit = accepted.clone();

    git(r.local(), &["switch", "rolling"]);
    std::fs::write(r.local().join("shared"), "top evolved\nmiddle\nbottom\n").unwrap();
    git(r.local(), &["commit", "-am", "rolling evolves merged line"]);
    git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
    let landed = git(r.local(), &["rev-parse", "HEAD"]);
    assert_eq!(
        std::fs::read_to_string(r.local().join("shared")).unwrap(),
        "top evolved\nmiddle\nbottom\nsource addition\n"
    );
    git::accepted_content(r.local(), &r.base, &accepted, &landed)
        .await
        .unwrap();

    git(r.local(), &["revert", "--no-edit", &source_edit]);
    let reverted = git(r.local(), &["rev-parse", "HEAD"]);
    let error = git::accepted_content(r.local(), &r.base, &accepted, &reverted)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_lost")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_uses_newest_rolling_merge_after_fast_forward_landing() {
    let r = repos();
    std::fs::write(r.local().join("shared"), "top\nmiddle\nbottom\n").unwrap();
    git(r.local(), &["add", "shared"]);
    git(r.local(), &["commit", "-m", "first rolling change"]);
    let first_rolling = git(r.local(), &["rev-parse", "HEAD"]);
    git(r.local(), &["switch", "source"]);
    git(r.local(), &["merge", "--no-edit", &first_rolling]);
    std::fs::write(
        r.local().join("shared"),
        "top\nmiddle\nbottom\nsource addition one\n",
    )
    .unwrap();
    git(r.local(), &["commit", "-am", "first source addition"]);

    git(r.local(), &["switch", "rolling"]);
    std::fs::write(r.local().join("shared"), "top evolved\nmiddle\nbottom\n").unwrap();
    git(r.local(), &["commit", "-am", "second rolling change"]);
    let second_rolling = git(r.local(), &["rev-parse", "HEAD"]);
    git(r.local(), &["switch", "source"]);
    git(r.local(), &["merge", "--no-edit", &second_rolling]);
    std::fs::write(
        r.local().join("shared"),
        "top evolved\nmiddle\nbottom\nsource addition one\nsource addition two\n",
    )
    .unwrap();
    git(r.local(), &["commit", "-am", "second source addition"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);
    assert_eq!(
        git::content_base(r.local(), &r.base, &accepted, &accepted)
            .await
            .unwrap(),
        second_rolling
    );

    git(r.local(), &["switch", "rolling"]);
    git(r.local(), &["merge", "--ff-only", &accepted]);
    std::fs::write(
        r.local().join("shared"),
        "top evolved\nmiddle evolved\nbottom\nsource addition one\nsource addition two\n",
    )
    .unwrap();
    git(
        r.local(),
        &["commit", "-am", "evolve same file after landing"],
    );
    let target = git(r.local(), &["rev-parse", "HEAD"]);
    git::accepted_content(r.local(), &r.base, &accepted, &target)
        .await
        .unwrap();

    git(r.local(), &["revert", "--no-edit", &accepted]);
    let reverted = git(r.local(), &["rev-parse", "HEAD"]);
    let error = git::accepted_content(r.local(), &r.base, &accepted, &reverted)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_lost")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn historical_landing_excludes_prior_published_work_and_survives_later_evolution() {
    let r = repos();
    git(r.local(), &["merge", "--no-ff", "--no-edit", &r.source]);
    let prior_landing = git(r.local(), &["rev-parse", "HEAD"]);
    git(r.local(), &["switch", "source"]);
    git(r.local(), &["merge", "--ff-only", &prior_landing]);
    std::fs::write(r.local().join("source"), "accepted new line\n").unwrap();
    git(r.local(), &["commit", "-am", "accepted later source"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "rolling"]);
    git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
    let landing = git(r.local(), &["rev-parse", "HEAD"]);
    assert_eq!(
        git::content_base(r.local(), &r.base, &accepted, &landing)
            .await
            .unwrap(),
        prior_landing
    );
    git::accepted_content(r.local(), &r.base, &accepted, &landing)
        .await
        .unwrap();
    r.publish(&landing);

    std::fs::write(r.local().join("source"), "later legitimate replacement\n").unwrap();
    git(r.local(), &["commit", "-am", "evolve after landing"]);
    let later = git(r.local(), &["rev-parse", "HEAD"]);
    r.publish(&later);
    verify_integration_target(r.local(), &accepted, &landing)
        .await
        .unwrap();
    git::accepted_content(r.local(), &r.base, &accepted, &landing)
        .await
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn historical_landing_checks_prior_source_at_first_crossing() {
    let r = repos();
    std::fs::write(r.local().join("shared"), "prior version\n").unwrap();
    git(r.local(), &["add", "shared"]);
    git(r.local(), &["commit", "-m", "common published content"]);
    let common = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "-c", "later-source"]);
    std::fs::write(r.local().join("prior"), "prior source content\n").unwrap();
    git(r.local(), &["add", "prior"]);
    git(r.local(), &["commit", "-m", "prior source content"]);
    let prior_source = git(r.local(), &["rev-parse", "HEAD"]);
    std::fs::write(r.local().join("current"), "current accepted content\n").unwrap();
    git(r.local(), &["add", "current"]);
    git(r.local(), &["commit", "-m", "current accepted content"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "rolling"]);
    std::fs::write(r.local().join("shared"), "later version\n").unwrap();
    git(r.local(), &["commit", "-am", "evolve common content"]);
    git(r.local(), &["merge", "--no-ff", "--no-edit", &prior_source]);
    assert_eq!(
        std::fs::read_to_string(r.local().join("prior")).unwrap(),
        "prior source content\n"
    );
    std::fs::write(r.local().join("prior"), "later prior evolution\n").unwrap();
    git(
        r.local(),
        &["commit", "-am", "evolve published prior source"],
    );
    git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
    let landing = git(r.local(), &["rev-parse", "HEAD"]);
    assert_eq!(
        git::content_base(r.local(), &r.base, &accepted, &landing)
            .await
            .unwrap(),
        prior_source
    );
    assert_eq!(
        git(r.local(), &["merge-base", &prior_source, &common]),
        common
    );
    git::accepted_content(r.local(), &r.base, &accepted, &landing)
        .await
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn historical_landing_checks_more_than_64_prefix_paths_at_first_merge() {
    for drop_prior_path in [false, true] {
        let r = repos();
        git(r.local(), &["switch", "source"]);
        for index in 0..80 {
            std::fs::write(r.local().join(format!("prior-{index}")), "prior source\n").unwrap();
        }
        git(r.local(), &["add", "."]);
        git(r.local(), &["commit", "-m", "long source prefix"]);
        let prior_source = git(r.local(), &["rev-parse", "HEAD"]);
        assert!(
            git(r.local(), &["diff", "--name-only", &r.base, &prior_source])
                .lines()
                .count()
                > 64
        );

        std::fs::write(r.local().join("current"), "new accepted content\n").unwrap();
        git(r.local(), &["add", "current"]);
        git(r.local(), &["commit", "-m", "one-file source continuation"]);
        let accepted = git(r.local(), &["rev-parse", "HEAD"]);
        assert_eq!(
            git(
                r.local(),
                &["diff", "--name-only", &prior_source, &accepted]
            ),
            "current"
        );

        git(r.local(), &["switch", "rolling"]);
        git(
            r.local(),
            &["merge", "--no-ff", "--no-commit", &prior_source],
        );
        if drop_prior_path {
            std::fs::remove_file(r.local().join("prior-0")).unwrap();
            git(r.local(), &["add", "-A"]);
        }
        git(r.local(), &["commit", "-m", "first rolling publication"]);
        let crossing = git(r.local(), &["rev-parse", "HEAD"]);
        let parents = git(r.local(), &["rev-list", "--parents", "-n", "1", &crossing]);
        assert_eq!(parents.split_whitespace().count(), 3);
        assert!(
            !git::ancestor(r.local(), &prior_source, &format!("{crossing}^1"))
                .await
                .unwrap()
        );
        assert!(
            git::ancestor(r.local(), &prior_source, &crossing)
                .await
                .unwrap()
        );

        git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
        let landing = git(r.local(), &["rev-parse", "HEAD"]);
        assert_eq!(
            git::content_base(r.local(), &r.base, &accepted, &landing)
                .await
                .unwrap(),
            prior_source
        );
        let proof = git::accepted_content(r.local(), &r.base, &accepted, &landing).await;
        if drop_prior_path {
            assert!(
                proof
                    .unwrap_err()
                    .to_string()
                    .contains("manager_v2_accepted_content_lost")
            );
        } else {
            proof.unwrap();
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn lead_branch_source_excludes_incoming_rolling_paths_at_landing() {
    let r = repos();
    for index in 0..65 {
        std::fs::write(r.local().join(format!("incoming-{index}")), "rolling\n").unwrap();
    }
    git(r.local(), &["add", "."]);
    git(r.local(), &["commit", "-m", "incoming rolling changes"]);
    let rolling_parent = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "source"]);
    git(r.local(), &["merge", "--no-edit", &rolling_parent]);
    std::fs::write(r.local().join("source"), "source addition\n").unwrap();
    git(r.local(), &["commit", "-am", "accepted source"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);
    git(r.local(), &["switch", "-c", "lead"]);
    std::fs::write(r.local().join("lead"), "lead continuation\n").unwrap();
    git(r.local(), &["add", "lead"]);
    git(r.local(), &["commit", "-m", "lead continuation"]);
    let lead = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "rolling"]);
    git(r.local(), &["merge", "--no-ff", "--no-edit", &lead]);
    let landing = git(r.local(), &["rev-parse", "HEAD"]);
    assert_eq!(
        git::content_base(r.local(), &r.base, &accepted, &landing)
            .await
            .unwrap(),
        rolling_parent
    );
    git::accepted_content(r.local(), &r.base, &accepted, &landing)
        .await
        .unwrap();
    r.publish(&landing);
    verify_integration_target(r.local(), &accepted, &landing)
        .await
        .unwrap();
    let source_parent_is_not_a_rolling_target =
        verify_integration_target(r.local(), &accepted, &accepted)
            .await
            .unwrap_err();
    assert!(
        source_parent_is_not_a_rolling_target
            .to_string()
            .contains("manager_v2_remote_target_mismatch")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn criss_cross_landing_checks_prior_content_evolved_on_either_side() {
    for scenario in ["rolling_evolved", "source_evolved", "landing_drops_prior"] {
        let r = repos();
        git(r.local(), &["switch", "source"]);
        std::fs::write(r.local().join("prior"), "prior source content\n").unwrap();
        git(r.local(), &["add", "prior"]);
        git(r.local(), &["commit", "-m", "prior source content"]);
        let prior_source = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "rolling"]);
        std::fs::write(r.local().join("rolling-only"), "rolling side\n").unwrap();
        git(r.local(), &["add", "rolling-only"]);
        git(r.local(), &["commit", "-m", "rolling side content"]);
        let rolling_side = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "source"]);
        git(r.local(), &["merge", "--no-ff", "--no-edit", &rolling_side]);
        if scenario != "rolling_evolved" {
            std::fs::write(
                r.local().join("prior"),
                "prior source content\nsource addition\n",
            )
            .unwrap();
            git(r.local(), &["commit", "-am", "evolve source prior content"]);
        }
        std::fs::write(r.local().join("feature"), "accepted feature\n").unwrap();
        git(r.local(), &["add", "feature"]);
        git(r.local(), &["commit", "-m", "accepted feature"]);
        let accepted = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "rolling"]);
        git(r.local(), &["merge", "--no-ff", "--no-edit", &prior_source]);
        assert_eq!(
            std::fs::read_to_string(r.local().join("prior")).unwrap(),
            "prior source content\n"
        );
        if scenario == "rolling_evolved" {
            std::fs::write(r.local().join("prior"), "later prior evolution\n").unwrap();
            git(
                r.local(),
                &["commit", "-am", "evolve published prior source"],
            );
        }
        let rolling_parent = git(r.local(), &["rev-parse", "HEAD"]);
        assert_eq!(
            git(
                r.local(),
                &["merge-base", "--all", &accepted, &rolling_parent]
            )
            .lines()
            .count(),
            2
        );
        if scenario == "landing_drops_prior" {
            git(r.local(), &["merge", "--no-ff", "--no-commit", &accepted]);
            std::fs::write(r.local().join("prior"), "source addition\n").unwrap();
            git(r.local(), &["add", "prior"]);
            git(r.local(), &["commit", "-m", "landing drops prior content"]);
        } else {
            git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
        }
        let landing = git(r.local(), &["rev-parse", "HEAD"]);
        let proof = git::accepted_content(r.local(), &r.base, &accepted, &landing).await;
        if scenario == "landing_drops_prior" {
            assert!(
                proof
                    .unwrap_err()
                    .to_string()
                    .contains("manager_v2_accepted_content_lost")
            );
        } else {
            proof.unwrap();
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn criss_cross_landing_checks_virtual_base_and_still_rejects_dropped_content() {
    for scenario in [
        "preserved",
        "dropped_feature",
        "dropped_source",
        "rolling_evolved",
    ] {
        let r = repos();
        std::fs::write(r.local().join("rolling-only"), "rolling\n").unwrap();
        git(r.local(), &["add", "rolling-only"]);
        git(r.local(), &["commit", "-m", "rolling side"]);
        let rolling_side = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "source"]);
        git(r.local(), &["merge", "--no-ff", "--no-edit", &rolling_side]);
        std::fs::write(r.local().join("feature"), "accepted feature\n").unwrap();
        git(r.local(), &["add", "feature"]);
        git(r.local(), &["commit", "-m", "accepted feature"]);
        let accepted = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "rolling"]);
        // Merge the old source side, not its current tip. The two branches now
        // have distinct merges of the same two commits (criss-cross history).
        git(r.local(), &["merge", "--no-ff", "--no-edit", &r.source]);
        let rolling_parent = git(r.local(), &["rev-parse", "HEAD"]);
        let bases = git(
            r.local(),
            &["merge-base", "--all", &accepted, &rolling_parent],
        );
        assert_eq!(bases.lines().count(), 2);
        if scenario != "preserved" {
            git(r.local(), &["merge", "--no-ff", "--no-commit", &accepted]);
            match scenario {
                "dropped_feature" => std::fs::remove_file(r.local().join("feature")).unwrap(),
                "dropped_source" => std::fs::remove_file(r.local().join("source")).unwrap(),
                "rolling_evolved" => {
                    std::fs::write(r.local().join("rolling-only"), "rolling evolved\n").unwrap()
                }
                _ => unreachable!(),
            }
            git(r.local(), &["add", "-A"]);
            git(r.local(), &["commit", "-m", "landing changes content"]);
        } else {
            git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
        }
        let landing = git(r.local(), &["rev-parse", "HEAD"]);
        let proof = git::accepted_content(r.local(), &r.base, &accepted, &landing).await;
        if scenario.starts_with("dropped_") {
            assert!(
                proof
                    .unwrap_err()
                    .to_string()
                    .contains("manager_v2_accepted_content_lost")
            );
        } else {
            proof.unwrap();
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn criss_cross_landing_admits_source_side_worker_merge_and_checks_its_content() {
    for drop_worker in [false, true] {
        let r = repos();
        git(r.local(), &["switch", "source"]);
        git(r.local(), &["switch", "-c", "worker"]);
        std::fs::write(r.local().join("worker"), "accepted worker\n").unwrap();
        git(r.local(), &["add", "worker"]);
        git(r.local(), &["commit", "-m", "worker content"]);
        git(r.local(), &["switch", "source"]);
        std::fs::write(r.local().join("lead"), "accepted lead\n").unwrap();
        git(r.local(), &["add", "lead"]);
        git(r.local(), &["commit", "-m", "lead content"]);
        git(r.local(), &["merge", "--no-ff", "--no-edit", "worker"]);
        let source_side_base = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "rolling"]);
        std::fs::write(r.local().join("rolling-only"), "rolling\n").unwrap();
        git(r.local(), &["add", "rolling-only"]);
        git(r.local(), &["commit", "-m", "rolling content"]);
        let rolling_side_base = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "source"]);
        git(
            r.local(),
            &["merge", "--no-ff", "--no-edit", &rolling_side_base],
        );
        std::fs::write(r.local().join("feature"), "accepted feature\n").unwrap();
        git(r.local(), &["add", "feature"]);
        git(r.local(), &["commit", "-m", "accepted feature"]);
        let accepted = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "rolling"]);
        git(
            r.local(),
            &["merge", "--no-ff", "--no-edit", &source_side_base],
        );
        let rolling_parent = git(r.local(), &["rev-parse", "HEAD"]);
        let bases = git(
            r.local(),
            &["merge-base", "--all", &accepted, &rolling_parent],
        );
        assert_eq!(bases.lines().count(), 2);
        if drop_worker {
            git(r.local(), &["merge", "--no-ff", "--no-commit", &accepted]);
            std::fs::remove_file(r.local().join("worker")).unwrap();
            git(r.local(), &["add", "-A"]);
            git(r.local(), &["commit", "-m", "landing drops worker content"]);
        } else {
            git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
        }
        let landing = git(r.local(), &["rev-parse", "HEAD"]);
        let proof = git::accepted_content(r.local(), &r.base, &accepted, &landing).await;
        if drop_worker {
            assert!(
                proof
                    .unwrap_err()
                    .to_string()
                    .contains("manager_v2_accepted_content_lost")
            );
        } else {
            proof.unwrap();
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn criss_cross_landing_refuses_unattributable_rolling_fast_forward() {
    for imported_side_branch in [false, true] {
        let r = repos();
        git(r.local(), &["merge", "--ff-only", "source"]);
        if imported_side_branch {
            git(r.local(), &["switch", "-c", "rolling-side"]);
            std::fs::write(r.local().join("rolling-side"), "rolling side content\n").unwrap();
            git(r.local(), &["add", "rolling-side"]);
            git(r.local(), &["commit", "-m", "rolling side content"]);
            git(r.local(), &["switch", "rolling"]);
        }
        std::fs::write(r.local().join("rolling-only"), "rolling\n").unwrap();
        git(r.local(), &["add", "rolling-only"]);
        git(r.local(), &["commit", "-m", "rolling content"]);
        let rolling_import = if imported_side_branch {
            git(
                r.local(),
                &["merge", "--no-ff", "--no-edit", "rolling-side"],
            );
            git(r.local(), &["rev-parse", "rolling-side"])
        } else {
            git(r.local(), &["rev-parse", "HEAD"])
        };

        git(r.local(), &["switch", "source"]);
        git(r.local(), &["merge", "--ff-only", &rolling_import]);
        std::fs::write(r.local().join("source-next"), "source next\n").unwrap();
        git(r.local(), &["add", "source-next"]);
        git(r.local(), &["commit", "-m", "source next"]);
        let source_side_base = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "rolling"]);
        std::fs::write(r.local().join("rolling-next"), "rolling next\n").unwrap();
        git(r.local(), &["add", "rolling-next"]);
        git(r.local(), &["commit", "-m", "rolling next"]);
        let rolling_side_base = git(r.local(), &["rev-parse", "HEAD"]);
        if imported_side_branch {
            let rolling_first_parent = git(
                r.local(),
                &["rev-list", "--first-parent", &rolling_side_base],
            );
            assert!(
                !rolling_first_parent
                    .lines()
                    .any(|sha| sha == rolling_import)
            );
        }

        git(r.local(), &["switch", "source"]);
        git(
            r.local(),
            &["merge", "--no-ff", "--no-edit", &rolling_side_base],
        );
        std::fs::write(r.local().join("feature"), "accepted feature\n").unwrap();
        git(r.local(), &["add", "feature"]);
        git(r.local(), &["commit", "-m", "accepted feature"]);
        let accepted = git(r.local(), &["rev-parse", "HEAD"]);

        git(r.local(), &["switch", "rolling"]);
        git(
            r.local(),
            &["merge", "--no-ff", "--no-edit", &source_side_base],
        );
        let rolling_parent = git(r.local(), &["rev-parse", "HEAD"]);
        assert_eq!(
            git(
                r.local(),
                &["merge-base", "--all", &accepted, &rolling_parent]
            )
            .lines()
            .count(),
            2
        );
        git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
        let landing = git(r.local(), &["rev-parse", "HEAD"]);
        let error = git::accepted_content(r.local(), &r.base, &accepted, &landing)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manager_v2_accepted_content_ambiguous")
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn criss_cross_landing_refuses_worker_import_of_rolling_history() {
    let r = repos();
    std::fs::write(r.local().join("rolling-only"), "rolling\n").unwrap();
    git(r.local(), &["add", "rolling-only"]);
    git(r.local(), &["commit", "-m", "rolling content"]);
    let rolling_import = git(r.local(), &["rev-parse", "HEAD"]);
    std::fs::write(r.local().join("rolling-next"), "rolling next\n").unwrap();
    git(r.local(), &["add", "rolling-next"]);
    git(r.local(), &["commit", "-m", "rolling next"]);
    let rolling_side_base = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "source"]);
    git(r.local(), &["switch", "-c", "worker"]);
    git(
        r.local(),
        &["merge", "--no-ff", "--no-edit", &rolling_import],
    );
    std::fs::write(r.local().join("worker"), "accepted worker\n").unwrap();
    git(r.local(), &["add", "worker"]);
    git(r.local(), &["commit", "-m", "worker content"]);
    git(r.local(), &["switch", "source"]);
    std::fs::write(r.local().join("lead"), "accepted lead\n").unwrap();
    git(r.local(), &["add", "lead"]);
    git(r.local(), &["commit", "-m", "lead content"]);
    git(r.local(), &["merge", "--no-ff", "--no-edit", "worker"]);
    let source_side_base = git(r.local(), &["rev-parse", "HEAD"]);
    git(
        r.local(),
        &["merge", "--no-ff", "--no-edit", &rolling_side_base],
    );
    std::fs::write(r.local().join("feature"), "accepted feature\n").unwrap();
    git(r.local(), &["add", "feature"]);
    git(r.local(), &["commit", "-m", "accepted feature"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "rolling"]);
    git(
        r.local(),
        &["merge", "--no-ff", "--no-edit", &source_side_base],
    );
    let rolling_parent = git(r.local(), &["rev-parse", "HEAD"]);
    assert_eq!(
        git(
            r.local(),
            &["merge-base", "--all", &accepted, &rolling_parent]
        )
        .lines()
        .count(),
        2
    );
    git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
    let landing = git(r.local(), &["rev-parse", "HEAD"]);
    let error = git::accepted_content(r.local(), &r.base, &accepted, &landing)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_ambiguous")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn historical_landing_rejects_source_content_lost_at_that_commit() {
    let r = repos();
    git(r.local(), &["merge", "--no-ff", "--no-commit", &r.source]);
    std::fs::remove_file(r.local().join("source")).unwrap();
    git(r.local(), &["add", "-A"]);
    git(r.local(), &["commit", "-m", "merge discards source file"]);
    let landing = git(r.local(), &["rev-parse", "HEAD"]);
    let error = git::accepted_content(r.local(), &r.base, &r.source, &landing)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_lost")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn historical_landing_retains_accepted_lines_across_target_insertions() {
    let r = repos();
    git(r.local(), &["switch", "source"]);
    std::fs::write(r.local().join("source"), "alpha\nbeta\n").unwrap();
    git(r.local(), &["commit", "-am", "accepted source lines"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "rolling"]);
    git(r.local(), &["merge", "--no-ff", "--no-commit", &accepted]);
    std::fs::write(r.local().join("source"), "alpha\ntarget addition\nbeta\n").unwrap();
    git(r.local(), &["add", "source"]);
    git(
        r.local(),
        &["commit", "-m", "land source with target addition"],
    );
    let landing = git(r.local(), &["rev-parse", "HEAD"]);

    git::accepted_content(r.local(), &r.base, &accepted, &landing)
        .await
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn later_landing_cannot_hide_lost_prior_source_content() {
    let r = repos();
    git(r.local(), &["merge", "--no-ff", "--no-commit", &r.source]);
    std::fs::remove_file(r.local().join("source")).unwrap();
    git(r.local(), &["add", "-A"]);
    git(
        r.local(),
        &["commit", "-m", "first landing drops source file"],
    );

    git(r.local(), &["switch", "source"]);
    std::fs::write(r.local().join("second"), "new accepted content\n").unwrap();
    git(r.local(), &["add", "second"]);
    git(r.local(), &["commit", "-m", "later source addition"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "rolling"]);
    git(r.local(), &["merge", "--no-ff", "--no-edit", &accepted]);
    let landing = git(r.local(), &["rev-parse", "HEAD"]);
    assert_eq!(
        git::content_base(r.local(), &r.base, &accepted, &landing)
            .await
            .unwrap(),
        r.source
    );
    assert_eq!(
        std::fs::read_to_string(r.local().join("second")).unwrap(),
        "new accepted content\n"
    );
    let error = git::accepted_content(r.local(), &r.base, &accepted, &landing)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_lost")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn landing_target_remains_valid_after_sandbox_fast_forward() {
    let r = repos();
    git(r.local(), &["merge", "--no-ff", "--no-edit", &r.source]);
    let landing = git(r.local(), &["rev-parse", "HEAD"]);
    r.publish(&landing);
    let url = verify_integration_target(r.local(), &r.source, &landing)
        .await
        .unwrap()
        .unwrap();

    git(r.local(), &["switch", "-c", "sandbox", &r.base]);
    git(r.local(), &["merge", "--no-ff", "--no-edit", &landing]);
    std::fs::write(r.local().join("sandbox"), "sandbox continuation\n").unwrap();
    git(r.local(), &["add", "sandbox"]);
    git(r.local(), &["commit", "-m", "sandbox continuation"]);
    let advanced = git(r.local(), &["rev-parse", "HEAD"]);
    r.publish(&advanced);
    let first_parent = git(r.local(), &["rev-list", "--first-parent", &advanced]);
    assert!(!first_parent.lines().any(|commit| commit == landing));

    verify_integration_target(r.local(), &r.source, &landing)
        .await
        .unwrap();
    integration_target_unchanged(r.local(), &r.source, &landing, Some(&url))
        .await
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn source_side_merge_is_not_a_historical_landing_target() {
    let r = repos();
    git(r.local(), &["switch", "source"]);
    git(r.local(), &["switch", "-c", "source-feature"]);
    std::fs::write(r.local().join("feature"), "feature\n").unwrap();
    git(r.local(), &["add", "feature"]);
    git(r.local(), &["commit", "-m", "source feature"]);
    git(r.local(), &["switch", "source"]);
    std::fs::write(r.local().join("continuation"), "continuation\n").unwrap();
    git(r.local(), &["add", "continuation"]);
    git(r.local(), &["commit", "-m", "source continuation"]);
    git(
        r.local(),
        &["merge", "--no-ff", "--no-edit", "source-feature"],
    );
    let source_side_merge = git(r.local(), &["rev-parse", "HEAD"]);

    git(r.local(), &["switch", "rolling"]);
    git(
        r.local(),
        &["merge", "--no-ff", "--no-edit", &source_side_merge],
    );
    let landing = git(r.local(), &["rev-parse", "HEAD"]);
    r.publish(&landing);
    let url = verify_integration_target(r.local(), &r.source, &landing)
        .await
        .unwrap()
        .unwrap();
    let error = verify_integration_target(r.local(), &r.source, &source_side_merge)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_remote_target_mismatch")
    );
    let recheck_error =
        integration_target_unchanged(r.local(), &r.source, &source_side_merge, Some(&url))
            .await
            .unwrap_err();
    assert!(
        recheck_error
            .to_string()
            .contains("manager_v2_remote_target_mismatch")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_admits_more_than_64_code_paths_with_later_evolution() {
    let r = repos();
    git(r.local(), &["switch", "source"]);
    std::fs::create_dir(r.local().join("thoughts")).unwrap();
    for index in 0..64 {
        std::fs::write(r.local().join(format!("thoughts/{index}.txt")), "note\n").unwrap();
    }
    git(r.local(), &["add", "thoughts"]);
    git(r.local(), &["commit", "-m", "source notes"]);
    let accepted = git(r.local(), &["rev-parse", "HEAD"]);
    git::accepted_content(r.local(), &r.base, &accepted, &accepted)
        .await
        .unwrap();

    for index in 0..65 {
        std::fs::write(r.local().join(format!("code-{index}.txt")), "code\n").unwrap();
    }
    git(r.local(), &["add", "."]);
    git(r.local(), &["commit", "-m", "many code paths"]);
    let many_code_paths = git(r.local(), &["rev-parse", "HEAD"]);
    git::accepted_content(r.local(), &r.base, &many_code_paths, &many_code_paths)
        .await
        .unwrap();

    for index in 0..65 {
        std::fs::write(
            r.local().join(format!("code-{index}.txt")),
            "code\nlater evolution\n",
        )
        .unwrap();
    }
    git(r.local(), &["commit", "-am", "evolve all code paths"]);
    let evolved = git(r.local(), &["rev-parse", "HEAD"]);
    git::accepted_content(r.local(), &r.base, &many_code_paths, &evolved)
        .await
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn remote_delivery_admits_local_lag_without_moving_local_rolling() {
    let r = repos();
    r.publish(&r.source);
    let before = git(r.local(), &["rev-parse", "refs/heads/rolling"]);
    let tracking_before = git(
        r.local(),
        &[
            "for-each-ref",
            "--format=%(refname):%(objectname)",
            "refs/remotes",
        ],
    );
    let url = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap()
        .unwrap();
    integration_target_unchanged(r.local(), &r.source, &r.source, Some(&url))
        .await
        .unwrap();
    assert_eq!(git(r.local(), &["rev-parse", "refs/heads/rolling"]), before);
    assert_eq!(before, r.base);
    assert_eq!(
        git(
            r.local(),
            &[
                "for-each-ref",
                "--format=%(refname):%(objectname)",
                "refs/remotes"
            ],
        ),
        tracking_before
    );
    assert_eq!(
        git(r.remote(), &["rev-parse", "refs/heads/rolling"]),
        r.source
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn remote_delivery_uses_default_dev_when_rolling_is_absent() {
    let r = repos();
    git(r.remote(), &["symbolic-ref", "HEAD", "refs/heads/dev"]);
    git(
        r.local(),
        &["push", "origin", &format!("{}:refs/heads/dev", r.source)],
    );
    let before = git(r.local(), &["rev-parse", "refs/heads/rolling"]);
    let url = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap()
        .unwrap();
    integration_target_unchanged(r.local(), &r.source, &r.source, Some(&url))
        .await
        .unwrap();
    assert_eq!(git::remote_head(r.local()).await.unwrap(), r.source);
    assert_eq!(git(r.local(), &["rev-parse", "refs/heads/rolling"]), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn remote_delivery_requires_a_resolved_default_without_rolling() {
    let r = repos();
    git(r.remote(), &["symbolic-ref", "HEAD", "refs/heads/dev"]);
    let error = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_v2_remote_unknown"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn remote_delivery_keeps_rolling_precedence_over_default_dev() {
    let r = repos();
    git(r.remote(), &["symbolic-ref", "HEAD", "refs/heads/dev"]);
    git(
        r.local(),
        &["push", "origin", &format!("{}:refs/heads/dev", r.source)],
    );
    r.publish(&r.base);
    let error = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_remote_target_mismatch")
    );
    r.publish(&r.source);
    verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn remote_mismatch_and_wrong_remote_are_refused() {
    let r = repos();
    r.publish(&r.base);
    git(r.local(), &["update-ref", "refs/heads/rolling", &r.source]);
    let error = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_remote_target_mismatch")
    );
    let wrong = r.dir.path().join("wrong.git");
    std::fs::create_dir(&wrong).unwrap();
    git(&wrong, &["init", "--bare"]);
    git(
        r.local(),
        &["remote", "set-url", "origin", wrong.to_str().unwrap()],
    );
    let error = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_v2_remote_unknown"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn wrong_repository_cannot_supply_a_remote_target() {
    let r = repos();
    r.publish(&r.source);
    let other = r.dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    git(&other, &["init", "-b", "rolling"]);
    let custody = crate::store::sandbox_custody::PersistedCustody {
        custody_id: Uuid::new_v4(),
        allocation_session_id: Uuid::new_v4(),
        allocation_id: Uuid::new_v4(),
        owner_session_id: Uuid::new_v4(),
        generation: 1,
        canonical_repo_dir: r.local.display().to_string(),
        sandbox_root: r.local.display().to_string(),
        sandbox_branch: "rolling".into(),
        repository_identity: std::fs::canonicalize(other.join(".git"))
            .unwrap()
            .display()
            .to_string(),
        source_commit: r.base.clone(),
    };
    let error = git::custody(&custody).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_custody_repository_changed")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn missing_source_and_unrelated_source_are_refused() {
    let r = repos();
    r.publish(&r.source);
    let error = verify_integration_target(r.local(), &"f".repeat(40), &r.source)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_source_missing")
    );
    git(r.local(), &["switch", "-c", "unrelated"]);
    std::fs::write(r.local().join("unrelated"), "unrelated").unwrap();
    git(r.local(), &["add", "unrelated"]);
    git(r.local(), &["commit", "-m", "unrelated"]);
    let unrelated = git(r.local(), &["rev-parse", "HEAD"]);
    let error = verify_integration_target(r.local(), &unrelated, &r.source)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_remote_ancestry_mismatch")
    );
    git(r.local(), &["switch", "--orphan", "orphan"]);
    git(r.local(), &["commit", "--allow-empty", "-m", "orphan"]);
    let orphan = git(r.local(), &["rev-parse", "HEAD"]);
    let error = verify_integration_target(r.local(), &orphan, &r.source)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_remote_ancestry_mismatch")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn remote_advance_after_evidence_preserves_the_landing_target() {
    let r = repos();
    r.publish(&r.source);
    let url = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap()
        .unwrap();
    git(r.local(), &["switch", "source"]);
    for index in 1..=2 {
        let path = format!("advance-{index}");
        std::fs::write(r.local().join(&path), format!("advance {index}\n")).unwrap();
        git(r.local(), &["add", &path]);
        git(r.local(), &["commit", "-m", &path]);
        let advanced = git(r.local(), &["rev-parse", "HEAD"]);
        r.publish(&advanced);
        integration_target_unchanged(r.local(), &r.source, &r.source, Some(&url))
            .await
            .unwrap();
        verify_integration_target(r.local(), &r.source, &r.source)
            .await
            .unwrap();
        assert_eq!(
            git(r.remote(), &["rev-parse", "refs/heads/rolling"]),
            advanced
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn remote_target_off_rolling_and_target_missing_source_are_refused() {
    let r = repos();
    r.publish(&r.source);
    git(r.local(), &["switch", "source"]);
    std::fs::write(r.local().join("other"), "other\n").unwrap();
    git(r.local(), &["add", "other"]);
    git(r.local(), &["commit", "-m", "unpublished target"]);
    let unpublished = git(r.local(), &["rev-parse", "HEAD"]);
    let off_rolling = verify_integration_target(r.local(), &r.source, &unpublished)
        .await
        .unwrap_err();
    assert!(
        off_rolling
            .to_string()
            .contains("manager_v2_remote_target_mismatch")
    );

    let missing_source = verify_integration_target(r.local(), &unpublished, &r.source)
        .await
        .unwrap_err();
    assert!(
        missing_source
            .to_string()
            .contains("manager_v2_remote_ancestry_mismatch")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn offline_and_missing_remote_refuse_local_equal_without_explicit_policy() {
    let r = repos();
    r.publish(&r.source);
    git(
        r.local(),
        &[
            "remote",
            "set-url",
            "origin",
            "/nonexistent/rsi-offline.git",
        ],
    );
    let error = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_v2_remote_unknown"));
    git(r.local(), &["update-ref", "refs/heads/rolling", &r.source]);
    let local_equal_offline = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap_err();
    assert!(
        local_equal_offline
            .to_string()
            .contains("manager_v2_remote_unknown")
    );
    git(r.local(), &["remote", "remove", "origin"]);
    let missing = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("manager_v2_remote_missing"));
    let local_equal_missing = verify_integration_target(r.local(), &r.source, &r.source)
        .await
        .unwrap_err();
    assert!(
        local_equal_missing
            .to_string()
            .contains("manager_v2_remote_missing")
    );
    git(
        r.local(),
        &[
            "config",
            "--local",
            "rsi.managerIntegrationTarget",
            "local-only",
        ],
    );
    assert!(
        verify_integration_target(r.local(), &r.source, &r.source)
            .await
            .unwrap()
            .is_none()
    );
    integration_target_unchanged(r.local(), &r.source, &r.source, None)
        .await
        .unwrap();
    git(
        r.local(),
        &[
            "config",
            "--local",
            "--unset",
            "rsi.managerIntegrationTarget",
        ],
    );
    let policy_removed = integration_target_unchanged(r.local(), &r.source, &r.source, None)
        .await
        .unwrap_err();
    assert!(
        policy_removed
            .to_string()
            .contains("manager_v2_local_only_policy_changed")
    );
    git(
        r.local(),
        &[
            "config",
            "--local",
            "rsi.managerIntegrationTarget",
            "local-only",
        ],
    );
    git(
        r.local(),
        &["remote", "add", "origin", r.remote().to_str().unwrap()],
    );
    let origin_added = integration_target_unchanged(r.local(), &r.source, &r.source, None)
        .await
        .unwrap_err();
    assert!(
        origin_added
            .to_string()
            .contains("manager_v2_local_only_policy_changed")
    );
    assert!(
        verify_integration_target(r.local(), &r.source, &r.source)
            .await
            .unwrap()
            .is_some()
    );
}
