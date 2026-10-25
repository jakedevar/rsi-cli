//! The portable-shell guard's tests, ported case for case from the Python suite
//! `scripts/tests/test_check_portable_shell.py` (local #322, hub #2978).

use std::path::{Path, PathBuf};

use super::*;
use crate::testutil::{git, repo_root, write};

fn rules(text: &str) -> Vec<&'static str> {
    scan_text(text, "<text>")
        .into_iter()
        .map(|f| f.rule)
        .collect()
}

fn lines(text: &str) -> Vec<usize> {
    scan_text(text, "<text>")
        .into_iter()
        .map(|f| f.line)
        .collect()
}

fn line_rules(text: &str) -> Vec<(usize, &'static str)> {
    scan_text(text, "<text>")
        .into_iter()
        .map(|f| (f.line, f.rule))
        .collect()
}

#[test]
fn every_form_in_the_issue_is_flagged() {
    let cases: &[(&str, &[&str])] = &[
        ("mktemp -t rsi-x", &["mktemp-t"]),
        ("sed -i '' 's/a/b/' f", &["sed-i-empty"]),
        ("sed -i 's/a/b/' f", &["sed-i-bare"]),
        ("sed -i -e 's/a/b/' f", &["sed-i-bare"]),
        ("stat -f %m f", &["stat-f"]),
        ("stat -c %Y f", &["stat-c"]),
        ("date -j -f %s 1", &["date-j"]),
        ("date -d @1 +%s", &["date-d"]),
        ("readlink -f \"$0\"", &["readlink-f"]),
        ("grep -P 'a(?=b)' f", &["grep-P"]),
        ("grep -oP 'a' f", &["grep-P"]),
        ("find . | xargs -r rm", &["xargs-r"]),
        ("declare -A map", &["declare-A"]),
        ("mapfile -t lines < f", &["mapfile"]),
        ("readarray -t lines < f", &["mapfile"]),
        ("x=${name,,}", &["case-mod"]),
        ("echo \"${name^^}\"", &["case-mod"]),
        ("wait -n", &["wait-n"]),
        ("local -n ref=$1", &["nameref"]),
    ];
    for (source, expected) in cases {
        assert_eq!(rules(&format!("{source}\n")), *expected, "{source}");
    }
}

#[test]
fn portable_spellings_pass() {
    let clean = [
        "mktemp \"${TMPDIR:-/tmp}/rsi.XXXXXX\"",
        "mktemp -d -t rsi.XXXXXX",
        "mktemp -d",
        "sed -i.bak 's/a/b/' f",
        "sed -E 's/a/b/' f",
        "sed -n 's/-i/x/p' f",
        "stat --version",
        "date -u +%s",
        "readlink \"$link\"",
        "grep -E 'a|b' f",
        "grep -r foo .",
        "xargs grep -r foo",
        "find . -print0 | xargs -0 rm",
        "declare -a plain",
        "x=${name:-default}",
        "echo ${#name}",
        "wait \"$pid\"",
        "echo 'sed -i is GNU only'",
        "echo \"use mapfile later\"",
        "# sed -i comment",
    ];
    for source in clean {
        assert_eq!(
            rules(&format!("{source}\n")),
            Vec::<&str>::new(),
            "{source}"
        );
    }
}

#[test]
fn forms_nested_in_pipes_substitutions_and_wrappers_are_found() {
    let cases: &[(&str, &[&str])] = &[
        ("x=$(readlink -f \"$0\")", &["readlink-f"]),
        ("x=\"$(stat -c %Y f)\"", &["stat-c"]),
        ("x=`readlink -f f`", &["readlink-f"]),
        ("ssh host 'sed -i s/a/b/ f'", &["sed-i-bare"]),
        ("ssh host sed -i s/a/b/ f", &["sed-i-bare"]),
        ("bash -c \"declare -A m\"", &["declare-A"]),
        ("sudo sed -i '' f", &["sed-i-empty"]),
        ("cat f | grep -P x | sort", &["grep-P"]),
        ("find . -exec sed -i s/a/b/ {} +", &["sed-i-bare"]),
        ("if grep -P x f; then echo y; fi", &["grep-P"]),
        ("FOO=1 mapfile -t a < f", &["mapfile"]),
    ];
    for (source, expected) in cases {
        assert_eq!(rules(&format!("{source}\n")), *expected, "{source}");
    }
}

#[test]
fn a_finding_names_the_portable_alternative() {
    for rule in RULES {
        assert!(rule.side == "bsd" || rule.side == "gnu");
        assert!(!rule.message.trim().is_empty());
    }
    let finding = scan_text("readlink -f x\n", "scripts/a.sh").remove(0);
    let line = format_finding(&finding);
    assert!(line.contains("scripts/a.sh:1: [readlink-f]"), "{line}");
    assert!(line.contains("GNU-only"));
    assert!(line.contains("pwd -P"));
}

#[test]
fn a_uname_if_block_allows_both_sides() {
    let source = "if [ \"$(uname -s)\" = Darwin ]; then\n  stat -f %m f\nelse\n  stat -c %Y f\nfi\nstat -c %Y g\n";
    assert_eq!(line_rules(source), vec![(6, "stat-c")]);
}

#[test]
fn a_uname_case_and_ostype_and_probe_variable() {
    let source = concat!(
        "case \"$(uname)\" in\n",
        "  Linux) sed -i 's/a/b/' f ;;\n",
        "  Darwin) sed -i '' 's/a/b/' f ;;\n",
        "esac\n",
        "if [[ \"$OSTYPE\" == darwin* ]]; then date -j -f %s 1; fi\n",
        "os=$(uname -s)\n",
        "if [ \"$os\" = Linux ]; then readlink -f x; fi\n",
        "readlink -f y\n",
    );
    assert_eq!(line_rules(source), vec![(8, "readlink-f")]);
}

#[test]
fn a_same_line_uname_probe_allows_the_command() {
    assert_eq!(
        lines("[ \"$(uname)\" = Linux ] && readlink -f x\nreadlink -f y\n"),
        vec![2]
    );
}

#[test]
fn nested_blocks_leave_the_probe_at_the_right_fi() {
    let source = concat!(
        "if [ \"$(uname)\" = Linux ]; then\n",
        "  if [ -n \"$x\" ]; then\n",
        "    readlink -f a\n",
        "  fi\n",
        "  readlink -f b\n",
        "fi\n",
        "readlink -f c\n",
    );
    assert_eq!(lines(source), vec![7]);
}

#[test]
fn file_marker_in_the_header_exempts_the_matching_side() {
    assert_eq!(
        rules("#!/bin/bash\n# portability: linux-only\nstat -c %Y f\nsed -i '' f\n"),
        vec!["sed-i-empty"]
    );
    assert_eq!(
        rules("#!/bin/bash\n# portability: macos-only\nstat -f %m f\nstat -c %Y f\n"),
        vec!["stat-c"]
    );
}

#[test]
fn line_markers_apply_to_their_line_and_the_next_code_line() {
    let source = concat!(
        "#!/bin/bash\n",
        "set -e\n",
        "readlink -f a  # portability: linux-only\n",
        "readlink -f b\n",
        "# portability: linux-only\n",
        "readlink -f c\n",
        "readlink -f d\n",
    );
    assert_eq!(lines(source), vec![4, 7]);
}

#[test]
fn a_marker_after_code_is_not_a_file_marker() {
    assert_eq!(
        lines("#!/bin/bash\nset -e\n# portability: linux-only\nreadlink -f a\nreadlink -f b\n"),
        vec![5]
    );
}

#[test]
fn a_gnu_and_bsd_form_joined_by_or_is_the_fallback_idiom() {
    let linked = [
        "owner() {\n  stat -c %u \"$1\" 2>/dev/null || stat -f %u \"$1\"\n}\n",
        "stat -c %Y f || stat -f %m f\n",
        "stat -f %m f || stat -c %Y f\n",
        "m=$(stat -c %Y f 2>/dev/null \\\n  || stat -f %m f)\n",
        "m=$(stat -c %Y f ||\n  stat -f %m f)\n",
        "m=$(stat -c %Y f 2>/dev/null) || m=$(stat -f %m f)\n",
        "[ -e f ] && stat -c %Y f || stat -f %m f\n",
        "stat -c %Y f ||  # BSD\n  stat -f %m f\n",
        "sed -i 's/a/b/' f || sed -i '' 's/a/b/' f\n",
        "date -d @1 +%s || date -j -r 1 +%s\n",
        "date -j -r 1 +%s ||\n  date -d @1 +%s\n",
    ];
    for source in linked {
        assert_eq!(rules(source), Vec::<&str>::new(), "{source}");
    }
}

#[test]
fn adjacent_independent_statements_are_both_flagged() {
    let separate: &[(&str, &[&str])] = &[
        ("stat -c %Y f\nstat -f %m f\n", &["stat-c", "stat-f"]),
        ("stat -c %Y f; stat -f %m f\n", &["stat-c", "stat-f"]),
        ("stat -c %Y f && stat -f %m f\n", &["stat-c", "stat-f"]),
        (
            "x=$(stat -c %Y f)\ny=$(stat -f %m f)\n",
            &["stat-c", "stat-f"],
        ),
        (
            "sed -i 's/a/b/' f\nsed -i '' 's/a/b/' f\n",
            &["sed-i-bare", "sed-i-empty"],
        ),
        ("date -d @1 +%s\ndate -j -r 1 +%s\n", &["date-d", "date-j"]),
        (
            "stat -c %Y f || true\nstat -f %m f\n",
            &["stat-c", "stat-f"],
        ),
        ("stat -c %Y f &\nstat -f %m f\n", &["stat-c", "stat-f"]),
        ("stat -c %Y f;\n  stat -f %m f\n", &["stat-c", "stat-f"]),
        ("stat -c %Y f\n\n\n\nstat -f %m f\n", &["stat-c", "stat-f"]),
    ];
    for (source, expected) in separate {
        assert_eq!(rules(source), *expected, "{source}");
    }
}

#[test]
fn a_linked_pair_does_not_hide_a_third_independent_form() {
    assert_eq!(
        line_rules("stat -c %Y a || stat -f %m a\nstat -c %Y b\n"),
        vec![(2, "stat-c")]
    );
}

#[test]
fn or_links_do_not_cross_tool_families() {
    assert_eq!(rules("stat -c %Y f || date -j\n"), vec!["date-j", "stat-c"]);
    assert_eq!(
        rules("stat -c %Y f || stat -c %Z f\n"),
        vec!["stat-c", "stat-c"]
    );
}

#[test]
fn heredoc_to_a_shell_is_scanned_and_to_another_language_is_not() {
    let source = concat!(
        "ssh host bash -s <<'EOF'\n",
        "readlink -f /x\n",
        "EOF\n",
        "python3 - <<'PY'\n",
        "import subprocess; subprocess.run(['readlink', '-f', 'x'])  # sed -i\n",
        "readlink -f /y\n",
        "PY\n",
        "cat <<EOF\n",
        "readlink -f in a message\n",
        "EOF\n",
        "cat > run.sh <<'EOF'\n",
        "mapfile -t a < f\n",
        "EOF\n",
        "readlink -f after\n",
    );
    assert_eq!(
        line_rules(source),
        vec![(2, "readlink-f"), (12, "mapfile"), (14, "readlink-f")]
    );
}

#[test]
fn tab_stripped_heredoc_delimiter() {
    assert_eq!(
        lines("cat <<-EOF\n\treadlink -f x\n\tEOF\nreadlink -f y\n"),
        vec![4]
    );
}

#[test]
fn multiline_quoted_text_is_not_code() {
    assert_eq!(
        lines("echo \"first\nreadlink -f inside the string\nlast\"\nreadlink -f real\n"),
        vec![4]
    );
}

#[test]
fn line_continuation_joins_a_command() {
    assert_eq!(rules("sed -i \\\n  's/a/b/' f\n"), vec!["sed-i-bare"]);
}

#[test]
fn arithmetic_and_param_expansion_do_not_break_the_lexer() {
    assert_eq!(
        rules("n=$((1 + 2))\nm=${#arr[@]}\nx=${y:-$(readlink -f z)}\n"),
        vec!["readlink-f"]
    );
}

/// A scratch repository root with `scripts/` and `tools/`.
struct TreeFixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl TreeFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::create_dir_all(root.join("tools")).unwrap();
        Self { _dir: dir, root }
    }

    fn write(&self, rel: &str, text: &str) {
        write(&self.root, rel, text);
    }

    fn baseline(&self, files: &serde_json::Value) {
        self.write(
            BASELINE,
            &serde_json::json!({"schema": 1, "files": files}).to_string(),
        );
    }

    /// (exit code, stdout + stderr) of `portable-shell --root <root> args...`.
    fn run_guard(&self, args: &[&str]) -> (i32, String) {
        let root = self.root.to_str().unwrap();
        let mut argv = vec!["--root", root];
        argv.extend(args);
        let (code, out, err) = crate::testutil::run(main, &argv, &Ctx::new(&self.root, &[]));
        (code, format!("{out}{err}"))
    }

    fn git(&self, args: &[&str]) {
        git(&self.root, args);
    }

    fn read_files(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.root.join(BASELINE)).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["files"].clone()
    }
}

#[test]
fn shell_scripts_by_suffix_and_shebang_only() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "true\n");
    t.write("scripts/tool", "#!/usr/bin/env bash\ntrue\n");
    t.write("scripts/py", "#!/usr/bin/env python3\nprint(1)\n");
    t.write("scripts/data.json", "{}\n");
    t.write("scripts/sub/b.bash", "true\n");
    t.write("scripts/tests/fixtures/bad.sh", "readlink -f x\n");
    t.write("tools/c.sh", "true\n");
    assert_eq!(
        candidate_files(&t.root, &[]),
        vec!["scripts/a.sh", "scripts/sub/b.bash", "scripts/tool"]
    );
}

#[test]
fn explicit_paths_are_filtered_to_scripts_shell_files() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "true\n");
    t.write("scripts/p.py", "x = 1\n");
    t.write("tools/c.sh", "true\n");
    let absolute = t.root.join("scripts/a.sh").to_str().unwrap().to_string();
    let paths: Vec<String> = [
        "scripts/a.sh",
        "scripts/p.py",
        "tools/c.sh",
        "scripts/missing.sh",
        absolute.as_str(),
    ]
    .iter()
    .map(|p| (*p).to_string())
    .collect();
    assert_eq!(candidate_files(&t.root, &paths), vec!["scripts/a.sh"]);
}

#[test]
fn new_use_above_the_baseline_fails_and_names_the_alternative() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "readlink -f x\nreadlink -f y\n");
    t.baseline(&serde_json::json!({"scripts/a.sh": 1}));
    let (code, output) = t.run_guard(&[]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("scripts/a.sh: 2 non-portable form(s), baseline allows 1"),
        "{output}"
    );
    assert!(output.contains("scripts/a.sh:2: [readlink-f]"), "{output}");
    assert!(output.contains("pwd -P"));
}

#[test]
fn a_new_file_with_a_use_fails_with_no_baseline_entry() {
    let t = TreeFixture::new();
    t.write("scripts/new.sh", "mapfile -t a < f\n");
    t.baseline(&serde_json::json!({}));
    let (code, output) = t.run_guard(&[]);
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("while IFS= read -r"), "{output}");
}

#[test]
fn counts_equal_to_the_baseline_pass() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "readlink -f x\n");
    t.baseline(&serde_json::json!({"scripts/a.sh": 1}));
    assert_eq!(t.run_guard(&[]).0, 0);
}

#[test]
fn a_stale_count_must_be_lowered() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "true\n");
    t.baseline(&serde_json::json!({"scripts/a.sh": 2, "scripts/gone.sh": 1}));
    let (code, output) = t.run_guard(&[]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("baseline allows 2 but only 0 remain"),
        "{output}"
    );
    assert!(
        output.contains("scripts/gone.sh: baseline allows 1"),
        "{output}"
    );
    assert!(
        output.contains("(`scripts/repo-guard portable-shell --refresh`)"),
        "{output}"
    );
}

#[test]
fn explicit_paths_only_judge_those_files() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "true\n");
    t.write("scripts/b.sh", "readlink -f x\n");
    t.baseline(&serde_json::json!({}));
    assert_eq!(t.run_guard(&["scripts/a.sh"]).0, 0);
    assert_eq!(t.run_guard(&["scripts/b.sh"]).0, 1);
}

#[test]
fn a_missing_baseline_counts_every_use_as_new() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "readlink -f x\n");
    assert_eq!(t.run_guard(&[]).0, 1);
}

#[test]
fn a_marker_or_uname_branch_keeps_the_guard_green() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "# portability: linux-only\nreadlink -f x\n");
    t.write(
        "scripts/b.sh",
        "if [ \"$(uname)\" = Linux ]; then readlink -f x; fi\n",
    );
    t.baseline(&serde_json::json!({}));
    assert_eq!(t.run_guard(&[]).0, 0);
}

#[test]
fn the_real_baseline_matches_the_real_tree() {
    let root = repo_root();
    let root_text = root.to_str().unwrap();
    let (code, out, err) =
        crate::testutil::run(main, &["--root", root_text], &Ctx::new(&root, &[]));
    assert_eq!(code, 0, "{out}{err}");
}

#[test]
fn refresh_creates_then_lowers_but_never_raises() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "readlink -f x\nreadlink -f y\n");
    assert_eq!(t.run_guard(&["--refresh"]).0, 0);
    assert_eq!(t.read_files(), serde_json::json!({"scripts/a.sh": 2}));
    t.write("scripts/a.sh", "readlink -f x\n");
    assert_eq!(t.run_guard(&["--refresh"]).0, 0);
    assert_eq!(t.read_files(), serde_json::json!({"scripts/a.sh": 1}));
    t.write("scripts/a.sh", "readlink -f x\nreadlink -f y\n");
    let (code, output) = t.run_guard(&["--refresh"]);
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("never raises"), "{output}");
    assert_eq!(t.read_files(), serde_json::json!({"scripts/a.sh": 1}));
}

#[test]
fn refresh_drops_files_with_no_remaining_uses() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "true\n");
    t.baseline(&serde_json::json!({"scripts/a.sh": 3}));
    assert_eq!(t.run_guard(&["--refresh"]).0, 0);
    assert_eq!(t.read_files(), serde_json::json!({}));
}

#[test]
fn the_written_baseline_is_byte_identical_to_the_committed_layout() {
    let t = TreeFixture::new();
    t.write("scripts/a.sh", "readlink -f x\n");
    assert_eq!(t.run_guard(&["--refresh"]).0, 0);
    let text = std::fs::read_to_string(t.root.join(BASELINE)).unwrap();
    assert!(
        text.starts_with("{\n  \"schema\": 1,\n  \"about\": \"Accepted non-portable shell forms"),
        "{text}"
    );
    assert!(
        text.ends_with("\",\n  \"files\": {\n    \"scripts/a.sh\": 1\n  }\n}\n"),
        "{text}"
    );
    let committed = std::fs::read_to_string(repo_root().join(BASELINE)).unwrap();
    let about =
        |text: &str| serde_json::from_str::<serde_json::Value>(text).unwrap()["about"].clone();
    assert_eq!(
        about(&committed),
        about(&text),
        "the committed baseline carries the about text --refresh writes"
    );
}

/// A git repository on `rolling` with `scripts/a.sh` (one use) and its baseline.
fn base_ref_fixture() -> TreeFixture {
    let t = TreeFixture::new();
    t.git(&["init", "-q", "-b", "rolling"]);
    t.write("scripts/a.sh", "readlink -f x\n");
    t.baseline(&serde_json::json!({"scripts/a.sh": 1}));
    t.git(&["add", "."]);
    t.git(&["commit", "-q", "-m", "base"]);
    t
}

/// A second file whose baseline count went stale on rolling, outside this change.
fn stale_sibling(t: &TreeFixture) {
    t.write("scripts/b.sh", "true\n");
    t.baseline(&serde_json::json!({"scripts/a.sh": 1, "scripts/b.sh": 2}));
    t.git(&["add", "."]);
    t.git(&["commit", "-q", "-m", "stale sibling"]);
    t.write("scripts/c.sh", "true\n");
    t.git(&["add", "."]);
    t.git(&["commit", "-q", "-m", "unrelated change"]);
}

#[test]
fn a_raised_baseline_fails_even_when_the_tree_agrees() {
    let t = base_ref_fixture();
    t.write("scripts/a.sh", "readlink -f x\nreadlink -f y\n");
    t.baseline(&serde_json::json!({"scripts/a.sh": 2}));
    let (code, output) = t.run_guard(&["--base", "HEAD"]);
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("baseline raised from 1 to 2"), "{output}");
}

#[test]
fn a_new_baseline_entry_counts_as_a_raise() {
    let t = base_ref_fixture();
    t.write("scripts/b.sh", "readlink -f x\n");
    t.baseline(&serde_json::json!({"scripts/a.sh": 1, "scripts/b.sh": 1}));
    let (code, output) = t.run_guard(&["--base", "HEAD"]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("scripts/b.sh: baseline raised from 0 to 1"),
        "{output}"
    );
}

#[test]
fn an_unchanged_or_lowered_baseline_passes() {
    let t = base_ref_fixture();
    assert_eq!(t.run_guard(&["--base", "HEAD"]).0, 0);
    t.write("scripts/a.sh", "true\n");
    t.baseline(&serde_json::json!({}));
    assert_eq!(t.run_guard(&["--base", "HEAD"]).0, 0);
}

#[test]
fn an_untouched_file_with_a_stale_count_passes_in_diff_mode() {
    let t = base_ref_fixture();
    stale_sibling(&t);
    assert_eq!(
        t.run_guard(&["--base", "HEAD~2"]).0,
        1,
        "b.sh is touched by HEAD~1"
    );
    let (code, output) = t.run_guard(&["--base", "HEAD~1"]);
    assert_eq!(code, 0, "{output}");
    assert_eq!(
        t.run_guard(&[]).0,
        1,
        "a full-tree run still reports every stale count"
    );
}

#[test]
fn a_touched_file_with_a_stale_count_names_the_lower_baseline() {
    let t = base_ref_fixture();
    stale_sibling(&t);
    t.write("scripts/b.sh", "true\n# touched\n");
    t.git(&["add", "."]);
    t.git(&["commit", "-q", "-m", "touch b"]);
    let (code, output) = t.run_guard(&["--base", "HEAD~1"]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("lower the baseline for scripts/b.sh to 0"),
        "{output}"
    );
}

#[test]
fn a_new_use_in_any_file_fails_in_diff_mode() {
    let t = base_ref_fixture();
    stale_sibling(&t);
    t.write("scripts/c.sh", "readlink -f x\n");
    let (code, output) = t.run_guard(&["--base", "HEAD"]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("scripts/c.sh: 1 non-portable form(s), baseline allows 0"),
        "{output}"
    );
}

#[test]
fn a_deleted_file_with_a_baseline_count_must_be_lowered() {
    let t = base_ref_fixture();
    t.git(&["rm", "-q", "scripts/a.sh"]);
    t.git(&["commit", "-q", "-m", "delete a"]);
    let (code, output) = t.run_guard(&["--base", "HEAD~1"]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("lower the baseline for scripts/a.sh to 0"),
        "{output}"
    );
}

#[test]
fn changes_that_rolling_gained_after_the_fork_are_not_this_changes() {
    let t = base_ref_fixture();
    t.git(&["branch", "feature"]);
    t.write("scripts/a.sh", "true\n");
    t.baseline(&serde_json::json!({}));
    t.git(&["add", "."]);
    t.git(&["commit", "-q", "-m", "rolling fixes a.sh"]);
    t.git(&["checkout", "-q", "feature"]);
    t.write("scripts/d.sh", "true\n");
    t.git(&["add", "."]);
    t.git(&["commit", "-q", "-m", "feature work"]);
    let ctx = Ctx::new(&t.root, &[]);
    let expected: BTreeSet<String> = ["scripts/d.sh".to_string()].into();
    assert_eq!(changed_since(&ctx, &t.root, "rolling"), Some(expected));
}

#[test]
fn a_touched_list_scopes_the_stale_check_like_the_pre_push_hook() {
    let t = base_ref_fixture();
    stale_sibling(&t);
    let listing = t.root.join("touched.list");
    let listing_text = listing.to_str().unwrap();
    std::fs::write(&listing, b"scripts/c.sh\0").unwrap();
    assert_eq!(t.run_guard(&["--touched-from", listing_text]).0, 0);
    std::fs::write(&listing, b"scripts/c.sh\0scripts/b.sh\0").unwrap();
    let (code, output) = t.run_guard(&["--touched-from", listing_text]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("lower the baseline for scripts/b.sh to 0"),
        "{output}"
    );
    std::fs::write(&listing, b"").unwrap();
    assert_eq!(t.run_guard(&["--touched-from", listing_text]).0, 0);
}

#[test]
fn an_unreadable_base_falls_back_to_judging_every_count() {
    let t = base_ref_fixture();
    stale_sibling(&t);
    let (code, output) = t.run_guard(&["--base", "no-such-ref"]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("lower the baseline for scripts/b.sh"),
        "{output}"
    );
}

#[test]
fn a_base_without_a_baseline_file_is_skipped() {
    let t = base_ref_fixture();
    let ctx = Ctx::new(&t.root, &[]);
    assert_eq!(
        read_base_baseline(&ctx, &t.root, "HEAD", "tools/none.json"),
        Ok(None)
    );
    assert_eq!(
        read_base_baseline(&ctx, &t.root, "no-such-ref", BASELINE),
        Ok(None)
    );
}

#[test]
fn usage_errors_exit_two() {
    let t = TreeFixture::new();
    let (code, output) = t.run_guard(&["--refresh", "--list"]);
    assert_eq!(code, 2, "{output}");
    assert!(
        output.contains("not allowed with argument --refresh"),
        "{output}"
    );
}

#[test]
fn the_guard_is_registered_like_the_identity_guard() {
    let root = repo_root();
    let read = |rel: &str| std::fs::read_to_string(root.join(rel)).unwrap();
    assert!(read("tools/git-hooks/pre-push").contains("portable-shell"));
    assert!(read("tools/git-hooks/pre-commit").contains("portable-shell"));
    assert!(read("scripts/scoped-test").contains("\"portable_shell\""));
    assert!(read("scripts/check-portable-shell.sh").contains("repo-guard\" portable-shell"));
    assert!(Path::new(&root.join("scripts/repo-guard")).is_file());
}
