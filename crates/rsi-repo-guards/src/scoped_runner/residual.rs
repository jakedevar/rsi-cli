//! The residual (#366) lane: which mandatory residual suites a diff selects, and
//! what the runner can honestly say about each.
//!
//! The selection is `scripts/python-test-gate.py` ported exactly (changed tests
//! select themselves; `test_<stem>.py` matches a script stem; a test naming a
//! changed file's basename observes it; a path outside `scripts/` and `tools/`
//! selects a test naming its full repo-relative path; a test that declares
//! `# python-test-gate: scans <dir>` is selected by any change under that
//! directory; docs and thoughts select nothing). It reads the committed tree at
//! `head`, never the working tree, so a dry run may inspect another candidate.
//!
//! A suite is either a ported bash script (`test_<stem>.sh`, executed by
//! `residual_run`, which owns the verdict) or still a Python file. Python is
//! forbidden, so a selected `.py` suite is an `incomplete_contract` entry (red,
//! `executed_tests: 0`) naming the suite and its owner #366. A suite is ported
//! by adding the `.sh` suite and deleting the `.py` in one change; nothing is
//! skipped, waived or substituted by another runner.
use crate::touched_shards::git::Repository;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io;
use std::path::Path;

const SOURCE_ROOTS: [&str; 2] = ["scripts", "tools"];
const IGNORED_ROOTS: [&str; 2] = ["docs", "thoughts"];
const SUITE_ROOTS: [&str; 2] = ["scripts/tests", "tools/tests"];
/// The legacy marker (an unported suite keeps declaring its scan unchanged) and the
/// runner-neutral one new suites use; both read `# <marker> scans <dir>`.
const MARKERS: [&str; 2] = ["python-test-gate:", "suite-gate:"];

/// `Path.stem`: the name without its last suffix (a leading dot is not a suffix).
fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 => &name[..index],
        _ => name,
    }
}

/// The repo-relative test directories a test declares it scans whole.
fn scanned_directories(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix('#')?.trim_start();
            let rest = MARKERS
                .iter()
                .find_map(|marker| rest.strip_prefix(marker))?
                .trim_start();
            let rest = rest.strip_prefix("scans")?;
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            let token = rest.trim();
            if token.is_empty() || token.contains(char::is_whitespace) {
                return None;
            }
            Some(token.strip_suffix('/').unwrap_or(token).to_string())
        })
        .collect()
}

/// A bash suite opts into the residual lane with a `# suite-gate: tap` comment line.
fn declares_tap(text: &str) -> bool {
    text.lines().any(|line| {
        line.strip_prefix('#')
            .map(str::trim)
            .is_some_and(|rest| rest == "suite-gate: tap")
    })
}

fn under(path: &str, directory: &str) -> bool {
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `test` (with source `text`) covers the changed repo-relative `path`.
fn observes(test: &str, text: &str, path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    if SOURCE_ROOTS.contains(&first) {
        let name = path.rsplit('/').next().unwrap_or(path);
        let test_stem = stem(test.rsplit('/').next().unwrap_or(test));
        return path == test
            || test_stem == format!("test_{}", stem(name).replace('-', "_"))
            || text.contains(name);
    }
    text.contains(path)
}

fn is_suite(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    if !(name.starts_with("test_") && (name.ends_with(".py") || name.ends_with(".sh"))) {
        return false;
    }
    SUITE_ROOTS.iter().any(|root| under(path, root))
        || (path.starts_with("tools/") && path.matches('/').count() == 1)
}

/// The suites the diff selects, sorted, from the committed trees.
pub fn selected(repo: &Path, base: &str, head: &str) -> io::Result<Vec<String>> {
    let repository = Repository::new(repo)?;
    let diff = repository.text(&["diff", "--name-only", "--no-renames", "-z", base, head])?;
    let paths: Vec<&str> = diff
        .split('\0')
        .filter(|p| !p.is_empty())
        .filter(|p| !IGNORED_ROOTS.contains(&p.split('/').next().unwrap_or_default()))
        .collect();
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let suites: BTreeSet<String> = repository
        .files(head)?
        .into_iter()
        .filter(|p| is_suite(p))
        .collect();
    let names: Vec<String> = suites.iter().cloned().collect();
    let texts = repository.blobs(head, &names)?;
    Ok(suites
        .into_iter()
        .filter(|test| {
            let text = texts.get(test).map_or("", String::as_str);
            // A bash file is a suite only when it declares the TAP protocol: the older
            // exit-status shell tests (run through `test_shell_tests.sh`) are not.
            if test.ends_with(".sh") && !declares_tap(text) {
                return false;
            }
            let scans = scanned_directories(text);
            paths
                .iter()
                .any(|path| observes(test, text, path) || scans.iter().any(|dir| under(path, dir)))
        })
        .collect())
}

/// The residual lane's entries: a ported `.sh` suite is `planned` for `residual_run`
/// to execute; a still-Python suite is one red `incomplete_contract`.
pub fn lane(repo: &Path, base: &str, head: &str) -> io::Result<Vec<Value>> {
    Ok(selected(repo, base, head)?
        .into_iter()
        .map(|suite| {
            if suite.ends_with(".sh") {
                return super::residual_run::planned(&suite);
            }
            json!({
                "status": "incomplete_contract",
                "lane": "residual",
                "owner": "#366",
                "test_file": suite,
                "runner": null,
                "executed_tests": 0,
                "reason": "this mandatory residual suite is still a Python file; Python is forbidden and its Rust or shell port has not been published, so it cannot run and is never skipped, waived or run by another runner",
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn commit(dir: &Path, files: &[(&str, &str)], message: &str) -> String {
        for (name, text) in files {
            testutil::write(dir, name, text);
        }
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
    fn repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q", "-b", "main"]);
        let base = commit(
            dir.path(),
            &[
                ("scripts/tests/test_clean_target.py", "import unittest\n"),
                (
                    "scripts/tests/test_other.py",
                    "mentions crates/rsid/src/lib.rs\n",
                ),
                (
                    "scripts/tests/test_names_basename.py",
                    "uses cargo-slot here\n",
                ),
                (
                    "scripts/tests/test_scans.py",
                    "# python-test-gate: scans scripts/tests/\nimport unittest\n",
                ),
                ("tools/test_hooks.py", "unrelated\n"),
                ("tools/tests/test_nested.py", "unrelated\n"),
                ("scripts/clean-target", "x\n"),
                ("scripts/cargo-slot", "x\n"),
                ("crates/rsid/src/lib.rs", "x\n"),
                ("crates/other/src/lib.rs", "x\n"),
                ("docs/a.md", "x\n"),
            ],
            "base",
        );
        (dir, base)
    }
    fn after(dir: &Path, base: &str, edits: &[(&str, &str)]) -> Vec<String> {
        let head = commit(dir, edits, "edit");
        let chosen = selected(dir, base, &head).unwrap();
        testutil::git(dir, &["reset", "-q", "--hard", base]);
        chosen
    }

    #[test]
    fn a_script_selects_its_stem_test_and_any_test_that_names_its_basename() {
        let (dir, base) = repo();
        assert_eq!(
            after(dir.path(), &base, &[("scripts/clean-target", "y\n")]),
            ["scripts/tests/test_clean_target.py"]
        );
        assert_eq!(
            after(dir.path(), &base, &[("scripts/cargo-slot", "y\n")]),
            ["scripts/tests/test_names_basename.py"]
        );
    }

    #[test]
    fn a_path_outside_scripts_and_tools_selects_only_a_test_naming_its_full_path() {
        let (dir, base) = repo();
        assert_eq!(
            after(dir.path(), &base, &[("crates/rsid/src/lib.rs", "y\n")]),
            ["scripts/tests/test_other.py"]
        );
        assert!(after(dir.path(), &base, &[("crates/other/src/lib.rs", "y\n")]).is_empty());
    }

    #[test]
    fn docs_and_thoughts_select_nothing_and_a_changed_test_selects_itself() {
        let (dir, base) = repo();
        assert!(
            after(
                dir.path(),
                &base,
                &[("docs/a.md", "y\n"), ("thoughts/n.md", "y\n")]
            )
            .is_empty()
        );
        assert_eq!(
            after(dir.path(), &base, &[("tools/test_hooks.py", "changed\n")]),
            ["tools/test_hooks.py"]
        );
        assert_eq!(
            after(
                dir.path(),
                &base,
                &[("tools/tests/test_nested.py", "changed\n")]
            ),
            ["tools/tests/test_nested.py"]
        );
    }

    #[test]
    fn a_directory_scan_declaration_selects_the_suite_for_any_change_under_that_directory() {
        let (dir, base) = repo();
        let chosen = after(
            dir.path(),
            &base,
            &[("scripts/tests/fixtures/new.txt", "new\n")],
        );
        assert_eq!(chosen, ["scripts/tests/test_scans.py"]);
    }

    #[test]
    fn every_selected_suite_is_a_red_incomplete_contract_entry_naming_it() {
        let (dir, base) = repo();
        let head = commit(dir.path(), &[("scripts/clean-target", "y\n")], "edit");
        let entries = lane(dir.path(), &base, &head).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["status"], "incomplete_contract");
        assert_eq!(entries[0]["owner"], "#366");
        assert_eq!(
            entries[0]["test_file"],
            "scripts/tests/test_clean_target.py"
        );
        assert_eq!(entries[0]["executed_tests"], 0);
        // No selection, no entries: an empty lane is genuinely nothing selected.
        let docs = commit(dir.path(), &[("docs/a.md", "z\n")], "docs");
        assert!(lane(dir.path(), &head, &docs).unwrap().is_empty());
    }

    #[test]
    fn a_bash_suite_is_selected_like_a_python_one_only_when_it_declares_tap() {
        let (dir, _) = repo();
        let base = commit(
            dir.path(),
            &[
                ("scripts/fmt-changed.sh", "x\n"),
                (
                    "scripts/tests/test_fmt_changed.sh",
                    "#!/usr/bin/env bash\n# suite-gate: tap\n",
                ),
                (
                    "scripts/tests/test_rsid_health_wait.sh",
                    "#!/usr/bin/env bash\n",
                ),
                (
                    "scripts/tests/test_scan_sh.sh",
                    "# suite-gate: tap\n# suite-gate: scans scripts/tests/fixtures\n",
                ),
            ],
            "bash suites",
        );
        assert_eq!(
            after(dir.path(), &base, &[("scripts/fmt-changed.sh", "y\n")]),
            ["scripts/tests/test_fmt_changed.sh"]
        );
        // An exit-status shell test without the declaration is never a residual suite, even changed.
        assert_eq!(
            after(
                dir.path(),
                &base,
                &[("scripts/tests/test_rsid_health_wait.sh", "changed\n")]
            ),
            ["scripts/tests/test_scans.py"],
            "only the directory scanner observes it"
        );
        assert_eq!(
            after(
                dir.path(),
                &base,
                &[("scripts/tests/fixtures/new.txt", "new\n")]
            ),
            [
                "scripts/tests/test_scan_sh.sh",
                "scripts/tests/test_scans.py"
            ]
        );
        let head = commit(dir.path(), &[("scripts/fmt-changed.sh", "z\n")], "edit");
        let entries = lane(dir.path(), &base, &head).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["status"], "planned");
        assert_eq!(entries[0]["runner"], "bash");
        assert_eq!(entries[0]["test_file"], "scripts/tests/test_fmt_changed.sh");
    }
    #[test]
    fn the_stem_and_marker_parsing_match_the_gate() {
        assert_eq!(stem("foo.tar.gz"), "foo.tar");
        assert_eq!(stem(".hidden"), ".hidden");
        assert_eq!(stem("check-touched-shards"), "check-touched-shards");
        assert_eq!(
            scanned_directories(
                "# python-test-gate: scans scripts/tests\n#python-test-gate:   scans   tools/tests/ \n  # python-test-gate: scans nope\n# python-test-gate: scans two words\nx = 1  # python-test-gate: scans nope\n"
            ),
            ["scripts/tests", "tools/tests"]
        );
        assert!(under("scripts/tests/a.py", "scripts/tests"));
        assert!(!under("scripts/tests", "scripts/tests"));
        assert!(!under("scripts/testsuite/a.py", "scripts/tests"));
    }
}
