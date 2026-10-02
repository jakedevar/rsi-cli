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
fn provisional_fixture(root: &Path, scenario: &str) -> serde_json::Value {
    const BUILD: &str = r#"import importlib.util, json, pathlib, shutil, sys, types
spec = importlib.util.spec_from_file_location('renumber_tests', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
t = module.ProvisionalMigrationTest('test_build_merge_uses_private_committer_identity')
t.setUp()
root = pathlib.Path(sys.argv[2])
root.mkdir(parents=True, exist_ok=True)
shutil.move(str(t.repo), str(root / 'repo'))
t.repo = root / 'repo'
t.temp = types.SimpleNamespace(name=str(root))
renumber = module.RENUMBER
if sys.argv[3] == 'same':
    source = t.source('alpha')
    worktree = root / 'source-alpha'
    t.write(worktree, 'docs/accepted.txt', 'accepted line\n')
    # Like #884's migration_allocation.rs: a protected section in an
    # undeclared file, appended last and reordered by the landing inventory.
    catalog = 'crates/rsid/src/store/alpha_catalog.rs'
    t.write(worktree, catalog, '// RSI-RELEASED-MIGRATION-BEGIN: alpha-catalog\npub const ALPHA: &str = "alpha";\n// RSI-RELEASED-MIGRATION-END: alpha-catalog\n')
    tracked = renumber.guard.tracked_source_paths(renumber.revision_inventory(t.repo, source))
    files = {name: (worktree / name).read_text() for name in tracked}
    files[catalog] = (worktree / catalog).read_text()
    t.write(worktree, renumber.MANIFEST, json.dumps(renumber.guard.inventory(files), indent=2) + '\n')
    t.git(worktree, 'add', 'docs/accepted.txt', catalog, renumber.MANIFEST)
    t.git(worktree, 'commit', '-q', '-m', 'accepted document')
    source = t.git(worktree, 'rev-parse', 'HEAD')
    cohort = 'crates/rsid/src/store/cohort_settlement.rs'
    t.write(t.repo, cohort, '// RSI-RELEASED-MIGRATION-BEGIN: target-extra\n// target\n// RSI-RELEASED-MIGRATION-END: target-extra\n')
    paths = [renumber.STORE, cohort, 'crates/rsid/src/store/tests.rs']
    files = {name: (t.repo / name).read_text() for name in paths}
    inventory = renumber.guard.inventory(files)
    t.write(t.repo, renumber.MANIFEST, json.dumps(inventory, indent=2) + '\n')
    t.git(t.repo, 'add', cohort, renumber.MANIFEST)
    t.git(t.repo, 'commit', '-q', '-m', 'independent protected section')
    target = t.git(t.repo, 'rev-parse', 'HEAD')
    unit, candidate = t.candidate(source, target)
    t.git(t.repo, 'switch', '--detach', candidate)
    t.write(t.repo, 'docs/extra.txt', 'extra\n')
    t.git(t.repo, 'add', 'docs/extra.txt')
    t.git(t.repo, 'commit', '-q', '-m', 'later descendant')
    descendant = t.git(t.repo, 'rev-parse', 'HEAD')
    t.git(t.repo, 'switch', '--detach', candidate)
    t.git(t.repo, 'rm', 'docs/accepted.txt')
    t.git(t.repo, 'commit', '-q', '-m', 'revert accepted document')
    reverted = t.git(t.repo, 'rev-parse', 'HEAD')
    t.git(t.repo, 'switch', '--detach', descendant)
    t.git(t.repo, 'revert', '-m', '1', '--no-edit', candidate)
    merge_reverted = t.git(t.repo, 'rev-parse', 'HEAD')
    # The lander regenerates the inventory from a Python set, so its section
    # order varies per process (hash randomization). Re-emit the landed
    # inventory with the same JSON content in an order that always differs from
    # the landed one: sorted, or reverse-sorted when the landed order already
    # is sorted (otherwise the transform is a no-op and there is nothing to
    # commit).
    t.git(t.repo, 'switch', '--detach', candidate)
    manifest = json.loads(renumber.show(t.repo, candidate, renumber.MANIFEST).decode())
    landed = list(manifest['protected_sections'].items())
    reordered = sorted(landed)
    if reordered == landed:
        reordered = sorted(landed, reverse=True)
    assert reordered != landed, 'the fixture needs at least two protected sections'
    manifest['protected_sections'] = dict(reordered)
    t.write(t.repo, renumber.MANIFEST, json.dumps(manifest, indent=2) + '\n')
    t.git(t.repo, 'add', renumber.MANIFEST)
    t.git(t.repo, 'commit', '-q', '-m', 'regenerate inventory order')
    reordered = t.git(t.repo, 'rev-parse', 'HEAD')
    print(json.dumps({'base': t.base, 'source': source, 'candidate': candidate,
                      'descendant': descendant, 'reverted': reverted,
                      'merge_reverted': merge_reverted, 'reordered': reordered,
                      'old': unit['old_version'], 'assigned': unit['new_version']}))
else:
    first = t.source('alpha')
    source = t.source('beta')
    _, prior = t.candidate(first, t.base)
    unit, candidate = t.candidate(source, prior)
    worktree = root / 'source-beta'
    declaration = 'tools/provisional-migrations/beta.json'
    data = json.loads((worktree / declaration).read_text())
    data['files'][0]['source_blob'] = 'sha256:' + '0' * 64
    t.write(worktree, declaration, json.dumps(data) + '\n')
    t.git(worktree, 'add', declaration)
    t.git(worktree, 'commit', '-q', '-m', 'mismatched mapping')
    mismatch_source = t.git(worktree, 'rev-parse', 'HEAD')
    tree = t.git(t.repo, 'rev-parse', candidate + '^{tree}')
    mismatch = t.git(t.repo, 'commit-tree', tree, '-p', prior, '-p', mismatch_source, '-m', 'mismatched candidate')
    t.git(worktree, 'rm', declaration)
    t.git(worktree, 'commit', '-q', '-m', 'missing mapping')
    missing_source = t.git(worktree, 'rev-parse', 'HEAD')
    missing = t.git(t.repo, 'commit-tree', tree, '-p', prior, '-p', missing_source, '-m', 'missing candidate')
    fake_manifest = json.loads(renumber.show(t.repo, candidate, renumber.MANIFEST).decode())
    fake_manifest['latest_schema_version'] = 999
    t.write(t.repo, renumber.MANIFEST, json.dumps(fake_manifest) + '\n')
    t.git(t.repo, 'add', renumber.MANIFEST)
    fake_tree = t.git(t.repo, 'write-tree')
    fake_l = t.git(t.repo, 'commit-tree', fake_tree, '-p', prior, '-p', source, '-m', 'fake L')
    print(json.dumps({'base': t.base, 'source': source, 'candidate': candidate,
                      'mismatch_source': mismatch_source, 'mismatch': mismatch,
                      'missing_source': missing_source, 'missing': missing,
                      'fake_l': fake_l, 'old': unit['old_version'],
                      'assigned': unit['new_version']}))
"#;
    let file = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/tests/test_rolling_migration_renumber.py"
    );
    let output = Command::new("python3")
        .args([
            "-I",
            "-B",
            "-c",
            BUILD,
            file,
            root.to_str().unwrap(),
            scenario,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
fn fixture_string(fixture: &serde_json::Value, key: &str) -> String {
    fixture[key].as_str().unwrap().to_owned()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
async fn expect_accepted_content_error(
    repo: &Path,
    base: &str,
    source: &str,
    target: &str,
    code: &str,
) {
    let error = git::accepted_content(repo, base, source, target)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(code), "expected {code}, got {error}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_proves_same_version_transform_at_landing_and_descendant() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = provisional_fixture(dir.path(), "same");
    let repo = dir.path().join("repo");
    let base = fixture_string(&fixture, "base");
    let source = fixture_string(&fixture, "source");
    let landing = fixture_string(&fixture, "candidate");
    let descendant = fixture_string(&fixture, "descendant");
    let reordered = fixture_string(&fixture, "reordered");
    assert_eq!(fixture["old"], fixture["assigned"]);
    // The accepted source's attributable paths, exactly as accepted_content selects them.
    let changed = git(&repo, &["diff", "--name-only", "-z", &base, &source]);
    let paths: Vec<&[u8]> = changed
        .as_bytes()
        .split(|byte: &u8| *byte == 0)
        .filter(|path: &&[u8]| !path.is_empty())
        .collect();
    // Like #884: the regenerated inventory reorders accepted entries, so the
    // unmapped line check alone refuses; the proven mapping accepts.
    let error = git::verify_content_paths(&repo, &base, &source, &reordered, &paths)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("manager_v2_accepted_content_lost"),
        "expected unmapped refusal, got {error}"
    );
    for target in [&landing, &descendant, &reordered] {
        git::accepted_content(&repo, &base, &source, target)
            .await
            .unwrap();
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_proves_renumbered_file_and_section() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = provisional_fixture(dir.path(), "renumber");
    let repo = dir.path().join("repo");
    assert_eq!(
        fixture["old"].as_i64().unwrap() + 1,
        fixture["assigned"].as_i64().unwrap()
    );
    git::accepted_content(
        &repo,
        &fixture_string(&fixture, "base"),
        &fixture_string(&fixture, "source"),
        &fixture_string(&fixture, "candidate"),
    )
    .await
    .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_provisional_transform_still_rejects_real_revert() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = provisional_fixture(dir.path(), "same");
    let repo = dir.path().join("repo");
    // Reverting the whole landing drops the migration and its inventory entries.
    expect_accepted_content_error(
        &repo,
        &fixture_string(&fixture, "base"),
        &fixture_string(&fixture, "source"),
        &fixture_string(&fixture, "merge_reverted"),
        "manager_v2_accepted_content_lost",
    )
    .await;
    // Dropping an unrelated accepted file after a proven landing is still loss.
    expect_accepted_content_error(
        &repo,
        &fixture_string(&fixture, "base"),
        &fixture_string(&fixture, "source"),
        &fixture_string(&fixture, "reverted"),
        "manager_v2_accepted_content_lost",
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_provisional_transform_still_rejects_dropped_accepted_line() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = provisional_fixture(dir.path(), "renumber");
    let repo = dir.path().join("repo");
    git(
        &repo,
        &[
            "switch",
            "--detach",
            "--discard-changes",
            &fixture_string(&fixture, "candidate"),
        ],
    );
    let store = repo.join("crates/rsid/src/store/mod.rs");
    let original = std::fs::read_to_string(&store).unwrap();
    assert_eq!(original.matches("// V131: beta migration\n").count(), 1);
    std::fs::write(&store, original.replace("// V131: beta migration\n", "")).unwrap();
    git(&repo, &["add", "crates/rsid/src/store/mod.rs"]);
    git(
        &repo,
        &["commit", "-q", "-m", "drop accepted migration line"],
    );
    let dropped = git(&repo, &["rev-parse", "HEAD"]);
    expect_accepted_content_error(
        &repo,
        &fixture_string(&fixture, "base"),
        &fixture_string(&fixture, "source"),
        &dropped,
        "manager_v2_accepted_content_lost",
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_provisional_mapping_is_exact_or_original_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = provisional_fixture(dir.path(), "renumber");
    let repo = dir.path().join("repo");
    let base = fixture_string(&fixture, "base");
    expect_accepted_content_error(
        &repo,
        &base,
        &fixture_string(&fixture, "mismatch_source"),
        &fixture_string(&fixture, "mismatch"),
        "manager_v2_provisional_proof_failed",
    )
    .await;
    expect_accepted_content_error(
        &repo,
        &base,
        &fixture_string(&fixture, "missing_source"),
        &fixture_string(&fixture, "missing"),
        "manager_v2_accepted_content_lost",
    )
    .await;
    let fake_l = fixture_string(&fixture, "fake_l");
    let parent = git(&repo, &["rev-parse", &format!("{fake_l}^1")]);
    let parent_manifest = git(
        &repo,
        &["show", &format!("{parent}:tools/released-migrations.json")],
    );
    let parent_latest = serde_json::from_str::<serde_json::Value>(&parent_manifest)
        .unwrap()["latest_schema_version"]
        .as_i64()
        .unwrap();
    let fake_manifest = git(
        &repo,
        &["show", &format!("{fake_l}:tools/released-migrations.json")],
    );
    let fake_latest =
        serde_json::from_str::<serde_json::Value>(&fake_manifest).unwrap()["latest_schema_version"]
            .as_i64()
            .unwrap();
    assert_ne!(parent_latest + 1, fake_latest);
    expect_accepted_content_error(
        &repo,
        &base,
        &fixture_string(&fixture, "source"),
        &fake_l,
        "manager_v2_provisional_proof_failed",
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_provisional_mapping_rejects_two_landings() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = provisional_fixture(dir.path(), "renumber");
    let repo = dir.path().join("repo");
    let source = fixture_string(&fixture, "source");
    let first = fixture_string(&fixture, "candidate");
    let parent = git(&repo, &["rev-parse", &format!("{first}^1")]);
    let first_tree = git(&repo, &["rev-parse", &format!("{first}^{{tree}}")]);
    let second = git(
        &repo,
        &[
            "commit-tree",
            &first_tree,
            "-p",
            &parent,
            "-p",
            &source,
            "-m",
            "second landing",
        ],
    );
    let join_tree = git(&repo, &["rev-parse", &format!("{second}^{{tree}}")]);
    let target = git(
        &repo,
        &[
            "commit-tree",
            &join_tree,
            "-p",
            &first,
            "-p",
            &second,
            "-m",
            "join two landings",
        ],
    );
    expect_accepted_content_error(
        &repo,
        &fixture_string(&fixture, "base"),
        &source,
        &target,
        "manager_v2_provisional_proof_ambiguous",
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn review_path_disjointness_is_proven_from_git_or_fails_closed() {
    // #984: a descendant is unrelated only when Git proves its own changes
    // touch no path of the reviewed range; a missing branch is an error.
    let r = repos();
    git(r.local(), &["switch", "-c", "sealed", &r.base]);
    std::fs::write(r.local().join("a.txt"), "sealed\n").unwrap();
    git(r.local(), &["add", "a.txt"]);
    git(r.local(), &["commit", "-m", "reviewed change"]);
    let sealed = git(r.local(), &["rev-parse", "HEAD"]);
    let unrelated = format!("rsi/{}", Uuid::new_v4());
    git(r.local(), &["switch", "-c", &unrelated, &r.base]);
    std::fs::write(r.local().join("b.txt"), "elsewhere\n").unwrap();
    git(r.local(), &["add", "b.txt"]);
    git(r.local(), &["commit", "-m", "unrelated child work"]);
    let overlapping = format!("rsi/{}", Uuid::new_v4());
    git(r.local(), &["switch", "-c", &overlapping, &r.base]);
    std::fs::write(r.local().join("a.txt"), "child\n").unwrap();
    git(r.local(), &["add", "a.txt"]);
    git(r.local(), &["commit", "-m", "overlapping child work"]);
    let range = git::changed_path_set(r.local(), &r.base, &sealed)
        .await
        .unwrap();
    let own = git::changed_path_set(r.local(), &r.base, &format!("refs/heads/{unrelated}"))
        .await
        .unwrap();
    assert!(own.is_disjoint(&range));
    let own = git::changed_path_set(r.local(), &r.base, &format!("refs/heads/{overlapping}"))
        .await
        .unwrap();
    assert!(!own.is_disjoint(&range));
    assert!(
        git::changed_path_set(
            r.local(),
            &r.base,
            &format!("refs/heads/rsi/{}", Uuid::new_v4())
        )
        .await
        .is_err()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[test]
fn accepted_content_diff_deadline_refuses_as_ambiguous() {
    // #978: the production diff bound still refuses; tests pass it explicitly.
    let (base, source, target) = (b"a\nb\n", b"a\nx\nb\n", b"a\nx\nb\ny\n");
    assert!(
        git::text_content_survives_within(base, source, target, std::time::Duration::from_secs(60))
            .unwrap()
    );
    let error = git::text_content_survives_within(base, source, target, std::time::Duration::ZERO)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_ambiguous"),
        "{error}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn accepted_content_time_bound_refuses_as_ambiguous() {
    // #978: an exhausted proof bound refuses with the same code; the proof
    // cannot finish without awaiting a Git subprocess.
    let r = repos();
    r.publish(&r.source);
    git::accepted_content_within(
        r.local(),
        &r.base,
        &r.source,
        &r.source,
        std::time::Duration::from_secs(600),
    )
    .await
    .unwrap();
    let error = git::accepted_content_within(
        r.local(),
        &r.base,
        &r.source,
        &r.source,
        std::time::Duration::ZERO,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_accepted_content_ambiguous"),
        "{error}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[test]
fn provisional_inventory_parsing_matches_released_migration_guard() {
    // Real store blocks are indented; the guard keeps each line ending.
    let store = b"fn migrate() {\n        if version < 6 {\n            six();\n        }\n        if version < 7 {  \r\n            seven();\n        }\r\n    }\n";
    let lines = git::normalized_lines(store).unwrap();
    let block = git::mapped_block(&lines, 7).unwrap();
    assert_eq!(
        block.concat(),
        b"        if version < 7 {  \r\n            seven();\n        }\r\n".to_vec()
    );
    assert!(git::normalized_lines(b"lone\rcarriage\n").is_err());
    let catalog = b"x\n  // RSI-RELEASED-MIGRATION-BEGIN: v7-catalog\nbody\n  // RSI-RELEASED-MIGRATION-END: v7-catalog  \n";
    let lines = git::normalized_lines(catalog).unwrap();
    assert_eq!(git::section_location(&lines, "v7-catalog").unwrap(), (1, 3));
    let duplicated = b"// RSI-RELEASED-MIGRATION-BEGIN: a\n// RSI-RELEASED-MIGRATION-BEGIN: a\n// RSI-RELEASED-MIGRATION-END: a\n";
    let lines = git::normalized_lines(duplicated).unwrap();
    assert!(git::section_location(&lines, "a").is_err());
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
