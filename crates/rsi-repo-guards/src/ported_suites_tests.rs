//! Native proof that the ported bash suites run green on the real checkout and go RED when
//! their subject is broken (#466, #475).
//!
//! The suites are bash TAP scripts (`scripts/tests/test_*.sh`, `tools/test_*.sh`). The review
//! found replacements that could not fail: a delimiter that silently widened a window, a missing
//! event read as zero, a commented key accepted as a parsed one. A repository without a
//! shell-suite recipe (#475) can still run them: every test here goes through the production
//! `residual_run::execute` (the verdict the lane uses), and every mutation runs in a SCRATCH COPY
//! of the subject, never in the checkout. Each mutation first runs the unmutated copy (it must be
//! green, or a red mutant proves nothing), then applies the edit (the text it replaces must exist,
//! or the mutation would be a no-op) and requires the suite to be red.
//!
//! Run as exact native jobs: package `rsi-repo-guards`, `lib_only`, `exact`, filters such as
//! `ported_suites_tests::green_socket_holders` (at most 32 filters per job).
use crate::pid_isolation::Isolation;
use crate::scoped_runner::{
    residual,
    residual_run::{Run, execute, planned},
};
use crate::testutil;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

/// What every scratch copy carries: the suites' libraries, scripts, tools and the files the
/// suites read from the repository root.
const CORE: [&str; 6] = [
    "scripts",
    "tools",
    ".rsi",
    "Makefile",
    "AGENTS.md",
    "docs/agents",
];

fn root() -> PathBuf {
    testutil::repo_root()
}

/// The environment the lane gives a suite: the runner's own allowlist (no `LANG`, no `USER`), so a
/// suite that silently relied on the operator's locale fails HERE and not only on the hub.
fn suite_env() -> BTreeMap<String, String> {
    crate::scoped_runner::process::build_env(&crate::Ctx::from_process())
}

/// Run one suite (`rel`, relative to `at`) through the lane's executor: the entry and its output.
fn run_suite(at: &Path, rel: &str) -> (Value, String) {
    let logs = tempfile::tempdir().unwrap();
    let env = suite_env();
    let isolation = Isolation::unavailable("ported_suites_tests");
    let cancel = AtomicUsize::new(0);
    let run = Run {
        work: at,
        env: &env,
        isolation: &isolation,
        cancel: &cancel,
        per_suite: Duration::from_secs(280),
        total_left: None,
        log_dir: logs.path(),
    };
    let mut said = String::new();
    let mut entries = execute(vec![planned(rel)], false, &run, &mut |line| {
        said.push_str(line);
        said.push('\n');
    })
    .unwrap();
    (entries.remove(0), said)
}

fn assert_green(at: &Path, rel: &str) {
    let (entry, said) = run_suite(at, rel);
    assert_eq!(entry["status"], "passed", "{rel}: {entry}\n{said}");
    assert!(
        entry["executed_tests"].as_u64().unwrap_or(0) > 0,
        "{rel} executed nothing: {entry}\n{said}"
    );
}

fn copy_into(scratch: &Path, rel: &str) {
    let from = root().join(rel);
    let to = scratch.join(rel);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    copy_path(&from, &to);
}

fn copy_path(from: &Path, to: &Path) {
    let meta = std::fs::symlink_metadata(from).unwrap();
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from).unwrap(), to).unwrap();
    } else if meta.is_dir() {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            copy_path(&entry.path(), &to.join(entry.file_name()));
        }
    } else {
        std::fs::copy(from, to).unwrap();
    }
}

/// A scratch checkout holding the core files (and `extra`), never a link into the real one.
fn scratch(extra: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for rel in CORE.iter().chain(extra) {
        if root().join(rel).exists() {
            copy_into(dir.path(), rel);
        }
    }
    dir
}

/// Replace every `from` in `rel` with `to`; the target MUST exist (a no-op mutation proves nothing).
fn mutate(dir: &Path, rel: &str, from: &str, to: &str) {
    let path = dir.join(rel);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(from),
        "mutation target {from:?} is not in {rel}"
    );
    std::fs::write(&path, text.replace(from, to)).unwrap();
}

/// The unmutated scratch copy is green; with the edits applied the suite is red.
fn assert_mutation_is_caught(
    suite: &str,
    edits: &[(&str, &str, &str)],
    extra: &[&str],
    what: &str,
) {
    let control = scratch(extra);
    let (entry, said) = run_suite(control.path(), suite);
    assert_eq!(
        entry["status"], "passed",
        "the UNMUTATED scratch copy of {suite} must be green before {what} means anything: {entry}\n{said}"
    );
    let mutant = scratch(extra);
    for (rel, from, to) in edits {
        mutate(mutant.path(), rel, from, to);
    }
    let (entry, said) = run_suite(mutant.path(), suite);
    assert_ne!(
        entry["status"], "passed",
        "{suite} stayed GREEN after {what}: {entry}\n{said}"
    );
    assert!(
        entry["failed_tests"].as_u64().unwrap_or(0) > 0,
        "{what}: {suite} went red for the wrong reason (no failed case): {entry}\n{said}"
    );
}

macro_rules! green {
    ($($name:ident => $suite:expr),+ $(,)?) => {$(
        #[test]
        fn $name() {
            assert_green(&root(), $suite);
        }
    )+};
}

// The 24 ported suites, run on the real checkout (executed evidence, not a source comparison).
green! {
    green_commit_msg => "tools/test_commit_msg.sh",
    green_git_rerere_guard => "tools/test_git_rerere_guard.sh",
    green_pre_commit_research_sidecar => "tools/test_pre_commit_research_sidecar.sh",
    green_wake_when_message_docs => "tools/test_wake_when_message_docs.sh",
    green_bash32_arrays => "scripts/tests/test_bash32_arrays.sh",
    green_build_aws_artifacts => "scripts/tests/test_build_aws_artifacts.sh",
    green_check_merge_whitespace => "scripts/tests/test_check_merge_whitespace.sh",
    green_check_release_seam => "scripts/tests/test_check_release_seam.sh",
    green_check_standalone_packages => "scripts/tests/test_check_standalone_packages.sh",
    green_clean_target => "scripts/tests/test_clean_target.sh",
    green_cloud_sweep_origins => "scripts/tests/test_cloud_sweep_origins.sh",
    green_desktop_e2e_graph_job => "scripts/tests/test_desktop_e2e_graph_job.sh",
    green_desktop_install_job => "scripts/tests/test_desktop_install_job.sh",
    green_fixture_git_guard => "scripts/tests/test_fixture_git_guard.sh",
    green_fmt_changed => "scripts/tests/test_fmt_changed.sh",
    green_ios_test_timeout => "scripts/tests/test_ios_test_timeout.sh",
    green_ios_weekly_agent => "scripts/tests/test_ios_weekly_agent.sh",
    green_rsi_stash => "scripts/tests/test_rsi_stash.sh",
    green_satellite_templates => "scripts/tests/test_satellite_templates.sh",
    green_seam_release_guard => "scripts/tests/test_seam_release_guard.sh",
    green_socket_holders => "scripts/tests/test_socket_holders.sh",
    green_systemd_test_fallback => "scripts/tests/test_systemd_test_fallback.sh",
    green_worker_briefs => "scripts/tests/test_worker_briefs.sh",
    green_run_rsid_test_shards => "scripts/tests/test_run_rsid_test_shards.sh",
    green_e2e_prebuild_single_definition => "scripts/tests/test_e2e_prebuild_single_definition.sh",
    green_shell_tests => "scripts/tests/test_shell_tests.sh",
}

// ---- #470: the REAL selector and dispatch for the wrapper's current inventory ----

/// The wrapper's `shell_test_subjects` table: `(shell test, subjects)`.
fn wrapper_inventory(text: &str) -> Vec<(String, Vec<String>)> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((name, rest)) = line.split_once(") echo \"") else {
            continue;
        };
        if !name.ends_with(".sh") {
            continue;
        }
        let Some((subjects, _)) = rest.split_once("\" ;;") else {
            continue;
        };
        found.push((
            name.to_string(),
            subjects.split_whitespace().map(str::to_string).collect(),
        ));
    }
    found
}

fn commit_all(dir: &Path, message: &str) -> String {
    testutil::git(dir, &["add", "-A"]);
    testutil::git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            message,
        ],
    );
    testutil::git(dir, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

fn selector_repo(
    wrapper_text: &str,
    inventory: &[(String, Vec<String>)],
) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
    testutil::write(
        dir.path(),
        "scripts/tests/test_shell_tests.sh",
        wrapper_text,
    );
    for (test, subjects) in inventory {
        let path = format!("scripts/tests/{test}");
        testutil::write(
            dir.path(),
            &path,
            &std::fs::read_to_string(root().join(&path)).unwrap(),
        );
        for subject in subjects {
            let path = format!("scripts/{subject}");
            testutil::write(
                dir.path(),
                &path,
                &std::fs::read_to_string(root().join(&path)).unwrap(),
            );
        }
    }
    let base = commit_all(dir.path(), "base");
    (dir, base)
}

fn touched(dir: &Path, rel: &str) -> String {
    let path = dir.join(rel);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n# touched\n");
    std::fs::write(&path, text).unwrap();
    commit_all(dir, "touch")
}

#[test]
fn the_residual_selector_picks_this_wrapper_for_every_listed_shell_test_and_subject() {
    const WRAPPER: &str = "scripts/tests/test_shell_tests.sh";
    let text = std::fs::read_to_string(root().join(WRAPPER)).unwrap();
    let inventory = wrapper_inventory(&text);
    assert!(
        inventory.len() >= 4,
        "the wrapper inventory was not read: {inventory:?}"
    );
    // #404: the ported install-release start suite is a listed exit-status shell test, with every
    // subject it exercises, so a change to the suite or to any subject selects the wrapper.
    let start = inventory
        .iter()
        .find(|(test, _)| test == "test_install_release_start.sh")
        .expect("test_install_release_start.sh must be listed in the wrapper");
    for subject in [
        "install-release.sh",
        "rsid-start-satellite",
        "rsid-health.sh",
        "rsid-launchd.sh",
        "rsid-processes.sh",
    ] {
        assert!(start.1.iter().any(|s| s == subject), "{subject}: {start:?}");
    }
    let (dir, base) = selector_repo(&text, &inventory);
    for (test, subjects) in &inventory {
        let mut changed = vec![format!("scripts/tests/{test}")];
        changed.extend(subjects.iter().map(|s| format!("scripts/{s}")));
        for path in changed {
            let head = touched(dir.path(), &path);
            let chosen = residual::selected(dir.path(), &base, &head).unwrap();
            assert!(
                chosen.iter().any(|s| s == WRAPPER),
                "a change to {path} must select {WRAPPER}, got {chosen:?}"
            );
            // Dispatch: the wrapper is planned for bash (a TAP suite), never a red contract gap.
            let lane = residual::lane(dir.path(), &base, &head).unwrap();
            let entry = lane.iter().find(|e| e["test_file"] == WRAPPER).unwrap();
            assert_eq!(entry["status"], "planned", "{path}: {entry}");
            assert_eq!(entry["runner"], "bash", "{path}: {entry}");
            testutil::git(dir.path(), &["reset", "-q", "--hard", &base]);
        }
    }
}

#[test]
fn host_warning_self_test_is_selected_and_run_by_the_rust_residual_lane() {
    const SUITE: &str = "test_check_host_warnings.sh";
    const SCRIPT: &str = "check-host-warnings.sh";
    let suite_path = format!("scripts/tests/{SUITE}");
    let wrapper_text =
        std::fs::read_to_string(root().join("scripts/tests/test_shell_tests.sh")).unwrap();
    let inventory = [(SUITE.to_owned(), vec![SCRIPT.to_owned()])];
    let (dir, base) = selector_repo(&wrapper_text, &inventory);
    let head = touched(dir.path(), &format!("scripts/{SCRIPT}"));
    let selected = residual::selected(dir.path(), &base, &head).unwrap();
    assert_eq!(selected, [suite_path.clone()]);
    assert_green(&root(), &suite_path);
}

/// The negative: a wrapper that stops naming a subject is no longer selected by it. The test above
/// is therefore a real constraint on the wrapper text, not a tautology.
#[test]
fn a_wrapper_that_stops_naming_a_subject_is_not_selected_by_it() {
    const WRAPPER: &str = "scripts/tests/test_shell_tests.sh";
    let text = std::fs::read_to_string(root().join(WRAPPER)).unwrap();
    let inventory = wrapper_inventory(&text);
    let (subject_owner, subjects) = inventory
        .iter()
        .find(|(_, subjects)| subjects.iter().any(|s| s == "rsid-supervisor.sh"))
        .expect("the quiet-restart entry lists rsid-supervisor.sh");
    let _ = subject_owner;
    let subject = subjects
        .iter()
        .find(|s| *s == "rsid-supervisor.sh")
        .unwrap();
    let broken = text.replace(subject.as_str(), "unrelated-subject.sh");
    let (dir, base) = selector_repo(&broken, &inventory);
    let head = touched(dir.path(), &format!("scripts/{subject}"));
    let chosen = residual::selected(dir.path(), &base, &head).unwrap();
    assert!(
        !chosen.iter().any(|s| s == WRAPPER),
        "the wrapper no longer names {subject}, yet it was selected: {chosen:?}"
    );
}

// ---- the harness helpers: adversarial suites on the real tap.sh ----

fn tap_suite(cases: &str) -> String {
    let lib = root().join("scripts/tests/lib/tap.sh");
    format!("source '{}'\n{cases}\ntap_done\n", lib.display())
}

fn tap_status(cases: &str) -> (Value, String) {
    let dir = tempfile::tempdir().unwrap();
    testutil::write(dir.path(), "t/test_adversarial.sh", &tap_suite(cases));
    run_suite(dir.path(), "t/test_adversarial.sh")
}

#[test]
fn file_bytes_are_compared_exactly() {
    let setup = "c_exact() { printf 'base\\n' >f; assert_file_is f $'base\\n' 'exact'; }\n\
                 c_nonl() { printf 'base' >f; assert_file_is f $'base\\n' 'missing newline'; }\n\
                 c_blank() { printf 'base\\n\\n' >f; assert_file_is f $'base\\n' 'extra blank line'; }\n\
                 c_missing() { assert_file_is nope $'base\\n' 'missing file'; }\n\
                 c_read() { printf 'a\\n\\n' >f; read_exact v f; [[ \"$v\" == $'a\\n\\n' ]] || tap_fail \"read_exact lost newlines: [$v]\"; }\n\
                 c_stdout() { run_capture printf 'x\\n'; assert_stdout_is $'x\\n'; assert_stdout_is 'x' 'no newline'; }\n\
                 run_case 'exact' c_exact\nrun_case 'nonl' c_nonl\nrun_case 'blank' c_blank\nrun_case 'missing' c_missing\n\
                 run_case 'read' c_read\nrun_case 'stdout' c_stdout\n";
    let (entry, said) = tap_status(setup);
    let red: Vec<&str> = said
        .lines()
        .filter_map(|l| l.strip_prefix("not ok "))
        .collect();
    assert_eq!(
        red,
        ["2 - nonl", "3 - blank", "4 - missing", "6 - stdout"],
        "{entry}\n{said}"
    );
    assert!(
        said.contains("ok 1 - exact") && said.contains("ok 5 - read"),
        "{said}"
    );
}

#[test]
fn ordering_toml_and_must_helpers_fail_closed() {
    let cases = "c_before_ok() { assert_before $'one\\ntwo\\n' one two 'order'; }\n\
                 c_before_missing_first() { assert_before $'two\\n' one two 'a missing first event'; }\n\
                 c_before_missing_second() { assert_before $'one\\n' one two 'a missing second event'; }\n\
                 c_before_wrong() { assert_before $'two\\none\\n' one two 'reversed'; }\n\
                 c_toml_ok() { printf '[a]\\nk = \"v\" # c\\n[b]\\nk = 2\\n' >t; assert_eq v \"$(toml_get t a k)\"; assert_eq 2 \"$(toml_get t b k)\"; }\n\
                 c_toml_commented() { printf '[a]\\n# k = \"v\"\\n' >t; x=\"$(toml_get t a k)\"; }\n\
                 c_toml_other_table() { printf '[b]\\nk = \"v\"\\n[a]\\n' >t; x=\"$(toml_get t a k)\"; }\n\
                 c_toml_duplicate() { printf '[a]\\nk = 1\\nk = 2\\n' >t; x=\"$(toml_get t a k)\"; }\n\
                 c_must_ok() { must true; }\n\
                 c_must_bad() { must false; }\n\
                 run_case 'a' c_before_ok\nrun_case 'b' c_before_missing_first\nrun_case 'c' c_before_missing_second\nrun_case 'd' c_before_wrong\n\
                 run_case 'e' c_toml_ok\nrun_case 'f' c_toml_commented\nrun_case 'g' c_toml_other_table\nrun_case 'h' c_toml_duplicate\n\
                 run_case 'i' c_must_ok\nrun_case 'j' c_must_bad\n";
    let (entry, said) = tap_status(cases);
    let red: Vec<&str> = said
        .lines()
        .filter_map(|l| l.strip_prefix("not ok "))
        .collect();
    assert_eq!(
        red,
        [
            "2 - b", "3 - c", "4 - d", "6 - f", "7 - g", "8 - h", "10 - j"
        ],
        "{entry}\n{said}"
    );
}

/// The names of the red cases of a TAP stream, as `<number> - <name>`.
fn red_cases(said: &str) -> Vec<&str> {
    said.lines()
        .filter_map(|l| l.strip_prefix("not ok "))
        .collect()
}

/// #483: TAP skip directives are case-insensitive, so a skip-only stream is never evidence.
#[test]
fn a_skip_only_stream_is_red_whatever_the_case_of_its_directive() {
    for directive in [
        "# SKIP no systemd",
        "# skip no systemd",
        "# Skip no systemd",
        "# sKiP no systemd",
        "#skip no systemd",
        "#   SKIPPED no systemd",
    ] {
        let dir = tempfile::tempdir().unwrap();
        testutil::write(
            dir.path(),
            "t/test_skip_only.sh",
            &format!("#!/usr/bin/env bash\nprintf 'ok 1 - unavailable {directive}\\n1..1\\n'\n"),
        );
        let (entry, said) = run_suite(dir.path(), "t/test_skip_only.sh");
        assert_ne!(entry["status"], "passed", "{directive}: {entry}\n{said}");
        assert_eq!(entry["executed_tests"], 0, "{directive}: {entry}\n{said}");
    }
    // A comment that is not a directive still counts as an executed case.
    let dir = tempfile::tempdir().unwrap();
    testutil::write(
        dir.path(),
        "t/test_comment.sh",
        "#!/usr/bin/env bash\nprintf 'ok 1 - fine # the skip list is unchanged\\nok 2 - plain\\n1..2\\n'\n",
    );
    let (entry, said) = run_suite(dir.path(), "t/test_comment.sh");
    assert_eq!(entry["status"], "passed", "{entry}\n{said}");
}

/// #480: a failed `cat` must not read as an empty value with a zero status.
#[test]
fn read_helpers_report_a_failed_read_instead_of_an_empty_value() {
    let cases = "c_read_fails() { : >f; cat() { return 7; }; read_exact v f; assert_eq '' \"$v\" 'an empty value'; }\n\
                 c_read_ok() { printf 'a\\n\\n' >f; read_exact v f; [[ \"$v\" == $'a\\n\\n' ]] || tap_fail \"lost newlines: [$v]\"; : >g; read_exact w g; assert_eq '' \"$w\" 'an empty file'; }\n\
                 c_capture_fails() { cat() { return 7; }; run_capture true; assert_code 0 'the command itself passed'; }\n\
                 c_capture_ok() { run_capture printf 'x\\n'; assert_eq x \"$RUN_OUT\"; assert_code 0; }\n\
                 run_case 'read fails' c_read_fails\nrun_case 'read ok' c_read_ok\n\
                 run_case 'capture fails' c_capture_fails\nrun_case 'capture ok' c_capture_ok\n";
    let (entry, said) = tap_status(cases);
    assert_eq!(
        red_cases(&said),
        ["1 - read fails", "3 - capture fails"],
        "{entry}\n{said}"
    );
    assert!(said.contains("cannot read"), "{said}");
}

/// #481: a malformed value is an error, never half a value; valid TOML keeps working.
#[test]
fn toml_get_rejects_malformed_values_and_keeps_valid_toml_working() {
    let cases = "c_unterminated() { printf '[recipes.x]\\nrunner = \"make\\ntarget = \"t\"\\n' >t; x=\"$(toml_get t recipes.x runner)\"; }\n\
                 c_garbage_after() { printf '[recipes.x]\\nrunner = \"make\" garbage\\n' >t; x=\"$(toml_get t recipes.x runner)\"; }\n\
                 c_literal_garbage() { printf \"[a]\\nk = 'make' x\\n\" >t; x=\"$(toml_get t a k)\"; }\n\
                 c_literal_open() { printf \"[a]\\nk = 'make\\n\" >t; x=\"$(toml_get t a k)\"; }\n\
                 c_bare_garbage() { printf '[a]\\nk = 1 2\\n' >t; x=\"$(toml_get t a k)\"; }\n\
                 c_array() { printf '[a]\\nk = [1, 2]\\n' >t; x=\"$(toml_get t a k)\"; }\n\
                 c_literal_ok() { printf \"[a]\\nk = 'make'\\n\" >t; assert_eq make \"$(toml_get t a k)\"; }\n\
                 c_comments_ok() { printf '[a] # the table\\nk = \"v\" # trailing\\nn = 5 # five\\n[b] \\nk = \"w#x\"\\n' >t; assert_eq v \"$(toml_get t a k)\"; assert_eq 5 \"$(toml_get t a n)\"; assert_eq 'w#x' \"$(toml_get t b k)\"; }\n\
                 c_dotted_ok() { printf '[recipes.desktop-e2e-graph]\\nrunner = \"make\"\\ntarget = \"desktop-e2e-graph-job\"\\n' >t; assert_eq make \"$(toml_get t recipes.desktop-e2e-graph runner)\"; assert_eq desktop-e2e-graph-job \"$(toml_get t recipes.desktop-e2e-graph target)\"; }\n\
                 run_case 'unterminated' c_unterminated\nrun_case 'garbage after' c_garbage_after\n\
                 run_case 'literal garbage' c_literal_garbage\nrun_case 'literal open' c_literal_open\n\
                 run_case 'bare garbage' c_bare_garbage\nrun_case 'array' c_array\n\
                 run_case 'literal ok' c_literal_ok\nrun_case 'comments ok' c_comments_ok\nrun_case 'dotted ok' c_dotted_ok\n";
    let (entry, said) = tap_status(cases);
    assert_eq!(
        red_cases(&said),
        [
            "1 - unterminated",
            "2 - garbage after",
            "3 - literal garbage",
            "4 - literal open",
            "5 - bare garbage",
            "6 - array"
        ],
        "{entry}\n{said}"
    );
    for ok in [
        "ok 7 - literal ok",
        "ok 8 - comments ok",
        "ok 9 - dotted ok",
    ] {
        assert!(said.contains(ok), "{ok}: {said}");
    }
}

// ---- mutation tests: each breaks a SUBJECT in a scratch copy and requires the suite red ----

#[test]
fn socket_holders_goes_red_when_the_intended_stop_announce_disappears() {
    assert_mutation_is_caught(
        "scripts/tests/test_socket_holders.sh",
        &[(
            "scripts/install-release.sh",
            "rsid_announce_intended_stop install_release",
            ": install_release",
        )],
        &[],
        "removing the intended-stop announce (a missing event must not read as offset zero)",
    );
}

#[test]
fn socket_holders_goes_red_when_the_launcher_snapshot_disappears() {
    assert_mutation_is_caught(
        "scripts/tests/test_socket_holders.sh",
        &[(
            "scripts/install-release.sh",
            "$(rsid_launcher_pids $old_pids)",
            "$(echo $old_pids)",
        )],
        &[],
        "removing the launcher snapshot",
    );
}

#[test]
fn socket_holders_goes_red_when_the_wait_for_the_old_supervisor_disappears() {
    assert_mutation_is_caught(
        "scripts/tests/test_socket_holders.sh",
        &[("scripts/install-release.sh", "sleep 0.1", ": 0.1")],
        &[],
        "removing the wait before the replacement starts",
    );
}

#[test]
fn socket_holders_goes_red_when_the_recovery_window_delimiter_disappears() {
    assert_mutation_is_caught(
        "scripts/tests/test_socket_holders.sh",
        &[(
            "scripts/systemd/README.md",
            "launcher_pid=",
            "launcher_ppid=",
        )],
        &[],
        "removing the recovery window's closing delimiter (the window must not widen)",
    );
}

#[test]
fn desktop_graph_goes_red_when_a_recipe_key_is_only_commented() {
    assert_mutation_is_caught(
        "scripts/tests/test_desktop_e2e_graph_job.sh",
        &[(
            ".rsi/jobs.toml",
            "[recipes.desktop-e2e-graph]\nrunner = \"make\"",
            "[recipes.desktop-e2e-graph]\n# runner = \"make\"",
        )],
        &[],
        "commenting out the recipe runner (the suite must read the ACTIVE key)",
    );
}

#[test]
fn desktop_graph_goes_red_when_the_judge_invocation_is_removed_but_its_filename_stays() {
    assert_mutation_is_caught(
        "scripts/tests/test_desktop_e2e_graph_job.sh",
        &[(
            "scripts/desktop-e2e-graph-job.sh",
            "node scripts/graph-run-receipt.mjs",
            "true scripts/graph-run-receipt.mjs",
        )],
        &[],
        "deleting the node invocation of the receipt judge while the basename remains",
    );
}

#[test]
fn desktop_graph_goes_red_when_the_raw_report_is_no_longer_echoed() {
    assert_mutation_is_caught(
        "scripts/tests/test_desktop_e2e_graph_job.sh",
        &[(
            "scripts/desktop-e2e-graph-job.sh",
            "cat \"$OUT/e2e-report.json\"; else",
            "true; else",
        )],
        &[],
        "dropping the raw report echo",
    );
}

#[test]
fn satellite_templates_go_red_when_a_later_duplicate_key_overrides_restart() {
    assert_mutation_is_caught(
        "scripts/tests/test_satellite_templates.sh",
        &[(
            "scripts/satellite/rsi-satellite-direct@.service",
            "Restart=always\n",
            "Restart=always\nRestart=no\n",
        )],
        &[],
        "appending `Restart=no` after `Restart=always` (the LAST duplicate key is effective)",
    );
}

#[test]
fn ios_weekly_agent_goes_red_when_start_interval_is_a_string_or_the_xml_is_malformed() {
    let suite = "scripts/tests/test_ios_weekly_agent.sh";
    assert_mutation_is_caught(
        suite,
        &[(
            "scripts/ios/install-weekly-agent.sh",
            "<integer>$interval_secs</integer>",
            "<string>$interval_secs</string>",
        )],
        &[],
        "emitting StartInterval as a string",
    );
    assert_mutation_is_caught(
        suite,
        &[(
            "scripts/ios/install-weekly-agent.sh",
            "</dict>\n</plist>",
            "</array>\n</plist>",
        )],
        &[],
        "closing the root dict with the wrong tag",
    );
    assert_mutation_is_caught(
        suite,
        &[(
            "scripts/ios/install-weekly-agent.sh",
            "<key>Label</key>",
            "<key>Labl</key>",
        )],
        &[],
        "renaming the Label key",
    );
}

/// #479 through the suite: a forged `Label` record and an unquoted attribute turn it red.
#[test]
fn ios_weekly_agent_goes_red_for_a_forged_label_record_or_an_unquoted_plist_attribute() {
    let suite = "scripts/tests/test_ios_weekly_agent.sh";
    let installer = "scripts/ios/install-weekly-agent.sh";
    // The genuine Label pair replaced by an `Other` string whose text carries a line break and
    // tabs, so the flattened record stream would hold `Label<TAB>string<TAB>$LABEL`.
    assert_mutation_is_caught(
        suite,
        &[(
            installer,
            "<key>Label</key>\n  <string>$LABEL</string>",
            "<key>Other</key>\n  <string>x\nLabel\tstring\t$LABEL</string>",
        )],
        &[],
        "forging a Label record inside another string's text",
    );
    assert_mutation_is_caught(
        suite,
        &[(installer, "<plist version=\"1.0\">", "<plist version=1.0>")],
        &[],
        "an unquoted plist version attribute",
    );
}

/// `lib/plist.awk` on its own: what it accepts, and what it refuses instead of misreading (#479).
fn plist_flatten(document: &str) -> (bool, String, String) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("awk")
        .arg("-f")
        .arg(root().join("scripts/tests/lib/plist.awk"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(document.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn plist_awk_records_are_unforgeable_and_malformed_xml_is_refused() {
    let wrap = |body: &str| format!("<plist><dict>{body}</dict></plist>\n");
    // The reviewer's forgery: text that looks like another key's record.
    let (ok, out, err) = plist_flatten(&wrap(
        "<key>Other</key><string>x\nLabel\tstring\tdev.rsi.ios-resign</string>",
    ));
    assert!(
        !ok,
        "a line break inside a string must be refused: {out}{err}"
    );
    assert!(
        !out.contains("Label\tstring"),
        "forged record leaked: {out}"
    );
    let (ok, out, err) = plist_flatten(&wrap(
        "<key>Other</key><string>x\tstring\ty</string><key>K</key><string>a</string>",
    ));
    assert!(!ok, "a tab inside a string must be refused: {out}{err}");
    let (ok, _, err) = plist_flatten(&wrap("<key>A\nLabel</key><string>y</string>"));
    assert!(!ok, "a line break inside a key must be refused: {err}");
    // A dotted key would collide with a nested path.
    let (ok, _, err) = plist_flatten(&wrap(
        "<key>EnvironmentVariables.HOME</key><string>y</string>",
    ));
    assert!(!ok, "a dotted key must be refused: {err}");
    // Attributes: only the exact quoted plist version.
    for bad in [
        "<plist version=1.0><dict><key>Label</key><string>x</string></dict></plist>\n",
        "<plist version='1.0'><dict><key>Label</key><string>x</string></dict></plist>\n",
        "<plist version=\"1.0\" extra=\"1\"><dict><key>Label</key><string>x</string></dict></plist>\n",
        "<plist><dict a=\"b\"><key>Label</key><string>x</string></dict></plist>\n",
        "<plist><dict><key id=\"1\">Label</key><string>x</string></dict></plist>\n",
    ] {
        let (ok, out, err) = plist_flatten(bad);
        assert!(!ok, "malformed attributes were accepted: {bad}{out}{err}");
    }
    // CDATA, numeric entities, unknown entities and duplicate keys are refused, not misread.
    for bad in [
        wrap("<key>Label</key><string><![CDATA[dev.rsi.ios-resign]]></string>"),
        wrap("<key>Label</key><string>&#100;ev.rsi</string>"),
        wrap("<key>Label</key><string>&nbsp;x</string>"),
        wrap("<key>Label</key><string>a & b</string>"),
        wrap("<key>Label</key><string>a</string><key>Label</key><string>b</string>"),
        wrap("<key>Label</key><string>a<string>b</string></string>"),
    ] {
        let (ok, out, err) = plist_flatten(&bad);
        assert!(!ok, "unsupported input was accepted: {bad}{out}{err}");
    }
    // What a real property list uses still parses, with exact typed records.
    let good = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>dev.rsi.ios-resign</string>\n\
        <key>Args</key><array><string>/bin/bash</string><string>a &amp; b &lt;c&gt;</string></array>\n\
        <key>StartInterval</key><integer>21600</integer>\n<key>RunAtLoad</key><true/>\n\
        <key>Blob</key><data>\nQUJD\n REVG\n</data>\n<!-- a comment -->\n\
        <key>Env</key><dict><key>HOME</key><string>/Users/x</string></dict>\n</dict>\n</plist>\n";
    let (ok, out, err) = plist_flatten(good);
    assert!(ok, "{err}");
    for record in [
        "Label\tstring\tdev.rsi.ios-resign\n",
        "Args.1\tstring\ta & b <c>\n",
        "StartInterval\tinteger\t21600\n",
        "RunAtLoad\ttrue\ttrue\n",
        "Blob\tdata\tQUJDREVG\n",
        "Env.HOME\tstring\t/Users/x\n",
    ] {
        assert!(out.contains(record), "missing {record:?} in {out:?}");
    }
}

#[test]
fn fixture_git_guard_goes_red_for_an_unsafe_new_fixture_a_dead_scanner_or_a_missing_allowance() {
    let suite = "scripts/tests/test_fixture_git_guard.sh";
    // A new shell fixture that runs Git after a bare cd.
    let control = scratch(&[]);
    testutil::write(
        control.path(),
        "scripts/tests/test_planted.sh",
        "cd \"$X\" && git commit -qm x\n",
    );
    let (entry, said) = run_suite(control.path(), suite);
    assert_ne!(
        entry["status"], "passed",
        "a planted unsafe fixture was accepted: {entry}\n{said}"
    );
    // A scanner that finds nothing.
    assert_mutation_is_caught(
        suite,
        &[(
            "scripts/tests/lib/fixture_git_scan.awk",
            "if (problem != \"\" && !allowed(i))",
            "if (0)",
        )],
        &[],
        "a scanner that reports nothing",
    );
    // An empty allowance inventory (the legacy list had exactly one entry).
    let pragma = "scripts/tests/test_scratch_git.py";
    assert_mutation_is_caught(
        suite,
        &[(pragma, "fixture-git: allow", "fixture-git: nope")],
        &[],
        "removing the only allowance (an empty inventory must fail)",
    );
}

// ---- a neutered subject: `exit 0` right after the shebang, for every script-backed suite ----

/// The suite is green on an unmutated scratch copy and red once `subject` does nothing.
fn assert_neutered_subject_is_caught(suite: &str, subject: &str) {
    let control = scratch(&[]);
    let (entry, said) = run_suite(control.path(), suite);
    assert_eq!(
        entry["status"], "passed",
        "unmutated scratch {suite}: {entry}\n{said}"
    );
    let mutant = scratch(&[]);
    let path = mutant.path().join(subject);
    let text = std::fs::read_to_string(&path).unwrap();
    let (shebang, rest) = text.split_once('\n').expect("a shebang line");
    assert!(shebang.starts_with("#!"), "{subject} has no shebang");
    std::fs::write(&path, format!("{shebang}\nexit 0\n{rest}")).unwrap();
    let (entry, said) = run_suite(mutant.path(), suite);
    assert_ne!(
        entry["status"], "passed",
        "{suite} stayed GREEN with {subject} neutered: {entry}\n{said}"
    );
    assert!(
        entry["failed_tests"].as_u64().unwrap_or(0) > 0,
        "{suite}: {entry}\n{said}"
    );
}

macro_rules! neutered {
    ($($name:ident => ($suite:expr, $subject:expr)),+ $(,)?) => {$(
        #[test]
        fn $name() {
            assert_neutered_subject_is_caught($suite, $subject);
        }
    )+};
}

neutered! {
    neutered_commit_msg_hook_turns_its_suite_red => ("tools/test_commit_msg.sh", "tools/git-hooks/commit-msg"),
    neutered_clean_target_turns_its_suite_red => ("scripts/tests/test_clean_target.sh", "scripts/clean-target"),
    neutered_check_merge_whitespace_turns_its_suite_red => ("scripts/tests/test_check_merge_whitespace.sh", "scripts/check-merge-whitespace"),
    neutered_fmt_changed_turns_its_suite_red => ("scripts/tests/test_fmt_changed.sh", "scripts/fmt-changed.sh"),
    neutered_systemd_test_fallback_turns_its_suite_red => ("scripts/tests/test_systemd_test_fallback.sh", "scripts/systemd-test-fallback"),
    neutered_rsi_stash_turns_its_suite_red => ("scripts/tests/test_rsi_stash.sh", "scripts/rsi-stash"),
    neutered_rsi_stash_turns_the_bash32_suite_red => ("scripts/tests/test_bash32_arrays.sh", "scripts/rsi-stash"),
    neutered_check_standalone_packages_turns_its_suite_red => ("scripts/tests/test_check_standalone_packages.sh", "scripts/check-standalone-packages.sh"),
    neutered_ios_test_turns_its_suite_red => ("scripts/tests/test_ios_test_timeout.sh", "scripts/ios/test.sh"),
    neutered_cloud_sweep_turns_its_suite_red => ("scripts/tests/test_cloud_sweep_origins.sh", "scripts/cloud-sweep.sh"),
    neutered_desktop_install_job_turns_its_suite_red => ("scripts/tests/test_desktop_install_job.sh", "scripts/desktop-install-job.sh"),
    neutered_run_rsid_test_shards_turns_its_suite_red => ("scripts/tests/test_run_rsid_test_shards.sh", "scripts/run-rsid-test-shards.sh"),
}

#[test]
fn commit_msg_goes_red_when_the_ai_trailer_is_no_longer_stripped() {
    assert_mutation_is_caught(
        "tools/test_commit_msg.sh",
        &[(
            "tools/git-hooks/commit-msg",
            "&& is_ai_trailer($0)) next",
            "&& 0) next",
        )],
        &[],
        "disabling the AI trailer stripping",
    );
}

#[test]
fn run_rsid_test_shards_goes_red_when_its_filterset_package_or_tier_handling_is_broken() {
    let suite = "scripts/tests/test_run_rsid_test_shards.sh";
    let subject = "scripts/run-rsid-test-shards.sh";
    assert_mutation_is_caught(
        suite,
        &[(
            subject,
            "[[ \"$filterset\" =~ $filterset_pattern ]] || usage",
            "true",
        )],
        &[],
        "accepting any filterset",
    );
    assert_mutation_is_caught(
        suite,
        &[(
            subject,
            "[[ -z \"$(printf '%s' \"$filterset\" | LC_ALL=C tr -d '\\040-\\176')\" ]] || usage",
            "true",
        )],
        &[],
        "dropping the control and non-ASCII refusal",
    );
    assert_mutation_is_caught(
        suite,
        &[(
            subject,
            "list_tier_args=(--ignore-default-filter)",
            "list_tier_args=()",
        )],
        &[],
        "listing with the default filter",
    );
    assert_mutation_is_caught(
        suite,
        &[(
            subject,
            "if (( slow_tier )); then run_tier_args=(--ignore-default-filter); fi",
            ":",
        )],
        &[],
        "a slow tier that no longer ignores the default filter",
    );
    assert_mutation_is_caught(
        suite,
        &[(
            subject,
            "flags+=\"-p $package_name \"",
            "flags+=\"-p rsid \"",
        )],
        &[],
        "selecting only one package for a shard",
    );
}

// ---- e2e_prebuild_single_definition (#488): a broken definition or consumer turns the suite red ----

/// The files the suite reads beyond the core copy: the list's consumers and the crates holding the
/// listed binaries.
const E2E_PREBUILD_EXTRA: [&str; 6] = [
    "crates/rsid/src/bin/rsi-rolling-land.rs",
    "crates/rsid/src/main.rs",
    "crates/rsi-common/src/bin",
    "crates/rsi-turn-shim/src/bin",
    "crates/rsi-repo-guards/src/scoped_runner/planner.rs",
    "crates/rsi-repo-guards/src/scoped_runner/runner.rs",
];

#[test]
fn e2e_prebuild_list_without_a_sibling_turns_its_suite_red() {
    assert_mutation_is_caught(
        "scripts/tests/test_e2e_prebuild_single_definition.sh",
        &[(
            "scripts/e2e-prebuild-bins.txt",
            "rsi-common rsi-agent-mcp\n",
            "",
        )],
        &E2E_PREBUILD_EXTRA,
        "dropping rsi-agent-mcp from the shared list",
    );
}

#[test]
fn a_lander_that_stops_including_the_list_turns_the_e2e_prebuild_suite_red() {
    assert_mutation_is_caught(
        "scripts/tests/test_e2e_prebuild_single_definition.sh",
        &[(
            "crates/rsid/src/bin/rsi-rolling-land.rs",
            "include_str!(\"../../../../scripts/e2e-prebuild-bins.txt\")",
            "include_str!(\"../../../../scripts/e2e-prebuild-bins.txt.old\")",
        )],
        &E2E_PREBUILD_EXTRA,
        "the lander including another file than the shared list",
    );
}

#[test]
fn an_e2e_script_that_ignores_the_list_turns_the_e2e_prebuild_suite_red() {
    assert_mutation_is_caught(
        "scripts/tests/test_e2e_prebuild_single_definition.sh",
        &[(
            "scripts/e2e.sh",
            "done < scripts/e2e-prebuild-bins.txt",
            "done < /dev/null",
        )],
        &E2E_PREBUILD_EXTRA,
        "e2e.sh reading no list",
    );
}
