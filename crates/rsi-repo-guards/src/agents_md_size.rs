//! Refuse a crate instruction file that grew past its size ceiling (#3450).
//!
//! Claude Code loads `crates/<crate>/CLAUDE.md` (a symlink to `AGENTS.md`) into the
//! context of every session that reads a file under that crate, so each byte costs
//! every manager and worker that touches the crate. `crates/rsid/AGENTS.md` had grown
//! to 62 KB (about 15k tokens per session) before this guard. A crate file keeps one
//! line per fact and points at `docs/agents/` for the detail.
//!
//! Checked: every regular `crates/*/AGENTS.md` and `crates/*/CLAUDE.md` (a symlink is
//! the same bytes as its target and is counted once, through the target). The default
//! ceiling is [`DEFAULT_CEILING`] bytes. A file over it needs an entry in
//! [`CEILINGS`], which may only go down: shrink the file, then lower the entry.
//!
//! Usage: `rsi-repo-guard agents-md-size [--root DIR]` (run from the repository
//! root; exit 1 on any finding, each printed on stderr).

use std::io::Write;
use std::path::Path;

use crate::Ctx;
use crate::cli::{self, Opt};

/// The ceiling of a crate instruction file without an entry in [`CEILINGS`].
pub const DEFAULT_CEILING: u64 = 12 * 1024;

/// Explicit ceilings (bytes) by repository-relative path. Lower an entry when its
/// file shrinks; never raise one. Move detail to `docs/agents/` instead.
pub const CEILINGS: &[(&str, u64)] = &[
    ("crates/rsid/AGENTS.md", 8 * 1024),
    ("crates/rsid-store/AGENTS.md", 17 * 1024),
];

const DETAIL_HINT: &str = "keep one line per fact in the crate file and move the detail to a section of \
     docs/agents/ (rsid: docs/agents/rsid-daemon-facts.md); never raise a ceiling";

/// The ceiling for the repository-relative `rel`.
#[must_use]
pub fn ceiling(rel: &str) -> u64 {
    CEILINGS
        .iter()
        .find(|(path, _)| *path == rel)
        .map_or(DEFAULT_CEILING, |(_, ceiling)| *ceiling)
}

/// A regular file (not a symlink): its size in bytes.
fn regular_file_size(path: &Path) -> Option<u64> {
    let link = std::fs::symlink_metadata(path).ok()?;
    if link.file_type().is_symlink() || !link.is_file() {
        return None;
    }
    Some(link.len())
}

/// Regular crate instruction files, as repository-relative posix paths: every
/// `crates/*/AGENTS.md` (sorted), then every `crates/*/CLAUDE.md` (sorted).
#[must_use]
pub fn instruction_files(root: &Path) -> Vec<String> {
    let mut crates: Vec<String> = std::fs::read_dir(root.join("crates"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    crates.sort();
    let mut found = Vec::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        for krate in &crates {
            let rel = format!("crates/{krate}/{name}");
            if regular_file_size(&root.join(&rel)).is_some() {
                found.push(rel);
            }
        }
    }
    found
}

/// Findings for crate instruction files over their ceiling, then for explicit
/// ceilings a file no longer needs.
#[must_use]
pub fn check(root: &Path) -> Vec<String> {
    let mut findings = Vec::new();
    for rel in instruction_files(root) {
        let Some(size) = regular_file_size(&root.join(&rel)) else {
            continue;
        };
        let ceiling = ceiling(&rel);
        if size > ceiling {
            findings.push(format!(
                "{rel}: {size} bytes is over its {ceiling}-byte ceiling; {DETAIL_HINT}"
            ));
        }
    }
    let mut explicit = CEILINGS.to_vec();
    explicit.sort_unstable();
    for (rel, ceiling) in explicit {
        if let Some(size) = regular_file_size(&root.join(rel))
            && size <= DEFAULT_CEILING
            && ceiling > DEFAULT_CEILING
        {
            findings.push(format!(
                "{rel}: fits the {DEFAULT_CEILING}-byte default; remove its CEILINGS entry"
            ));
        }
    }
    findings
}

/// Entry point: `agents-md-size [--root DIR]`.
pub fn main(args: &[String], ctx: &Ctx, _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let opts = [Opt {
        name: "--root",
        value: true,
    }];
    let parsed = match cli::parse(
        "check_agents_md_size",
        "[-h] [--root ROOT]",
        args,
        &opts,
        &[],
        Some(0),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    let findings = check(&ctx.path(parsed.value("--root").unwrap_or(".")));
    for finding in &findings {
        let _ = writeln!(err, "{finding}");
    }
    i32::from(!findings.is_empty())
}

// The cases of the replaced tools/test_check_agents_md_size.py, same inputs and
// expectations; plus the command-line contract (stderr, exit codes, --root).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn write(root: &Path, rel: &str, size: u64) -> std::path::PathBuf {
        testutil::write(root, rel, &"x".repeat(usize::try_from(size).unwrap()))
    }

    #[test]
    fn a_file_under_the_default_ceiling_passes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/demo/AGENTS.md", DEFAULT_CEILING);
        assert_eq!(check(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_file_over_the_default_ceiling_is_refused_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/demo/AGENTS.md", DEFAULT_CEILING + 1);
        let findings = check(dir.path());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].contains("crates/demo/AGENTS.md"),
            "{findings:?}"
        );
        assert!(findings[0].contains("docs/agents/"), "{findings:?}");
        assert_eq!(
            findings[0],
            "crates/demo/AGENTS.md: 12289 bytes is over its 12288-byte ceiling; keep one line \
             per fact in the crate file and move the detail to a section of docs/agents/ (rsid: \
             docs/agents/rsid-daemon-facts.md); never raise a ceiling"
        );
    }

    #[test]
    fn a_regular_claude_md_is_checked_too() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/tui/CLAUDE.md", DEFAULT_CEILING + 1);
        assert_eq!(check(dir.path()).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_md_symlink_is_counted_once_through_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = write(dir.path(), "crates/demo/AGENTS.md", DEFAULT_CEILING + 1);
        std::os::unix::fs::symlink("AGENTS.md", target.parent().unwrap().join("CLAUDE.md"))
            .unwrap();
        let findings = check(dir.path());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].contains("crates/demo/AGENTS.md"),
            "{findings:?}"
        );
    }

    #[test]
    fn an_explicit_ceiling_overrides_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let rsid = ceiling("crates/rsid/AGENTS.md");
        write(dir.path(), "crates/rsid/AGENTS.md", rsid);
        assert_eq!(check(dir.path()), Vec::<String>::new());
        write(dir.path(), "crates/rsid/AGENTS.md", rsid + 1);
        assert_eq!(check(dir.path()).len(), 1);
    }

    #[test]
    fn the_rsid_ceiling_is_the_issue_target_of_8_kib() {
        assert_eq!(ceiling("crates/rsid/AGENTS.md"), 8 * 1024);
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/rsid/AGENTS.md", 8 * 1024 + 1);
        assert_eq!(check(dir.path()).len(), 1);
    }

    #[test]
    fn a_stale_explicit_ceiling_for_a_small_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/rsid-store/AGENTS.md", 100);
        let findings = check(dir.path());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].contains("remove its CEILINGS entry"),
            "{findings:?}"
        );
        assert_eq!(
            findings[0],
            "crates/rsid-store/AGENTS.md: fits the 12288-byte default; remove its CEILINGS entry"
        );
    }

    #[test]
    fn the_repository_is_within_its_ceilings() {
        assert_eq!(check(&testutil::repo_root()), Vec::<String>::new());
    }

    #[test]
    fn the_rsid_file_is_a_short_index_with_the_detail_in_its_doc() {
        let repo = testutil::repo_root();
        let rsid = repo.join("crates/rsid/AGENTS.md");
        assert!(std::fs::metadata(&rsid).unwrap().len() <= 8 * 1024);
        assert!(repo.join("docs/agents/rsid-daemon-facts.md").is_file());
        assert!(
            std::fs::read_to_string(&rsid)
                .unwrap()
                .contains("docs/agents/rsid-daemon-facts.md")
        );
    }

    #[test]
    fn findings_follow_the_scan_order_agents_then_claude_then_stale_ceilings() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/rsid-store/AGENTS.md", 100);
        write(dir.path(), "crates/zeta/AGENTS.md", DEFAULT_CEILING + 2);
        write(dir.path(), "crates/rsid/AGENTS.md", 8 * 1024 + 3);
        write(dir.path(), "crates/alpha/CLAUDE.md", DEFAULT_CEILING + 4);
        let prefixes: Vec<String> = check(dir.path())
            .iter()
            .map(|finding| finding.split(':').next().unwrap().to_string())
            .collect();
        assert_eq!(
            prefixes,
            [
                "crates/rsid/AGENTS.md",
                "crates/zeta/AGENTS.md",
                "crates/alpha/CLAUDE.md",
                "crates/rsid-store/AGENTS.md",
            ]
        );
    }

    #[test]
    fn the_command_prints_findings_on_stderr_and_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/demo/AGENTS.md", DEFAULT_CEILING + 1);
        let ctx = Ctx::new(dir.path(), &[]);
        let (code, out, err) = testutil::run(main, &[], &ctx);
        assert_eq!((code, out.as_str()), (1, ""));
        assert!(
            err.starts_with("crates/demo/AGENTS.md: 12289 bytes is over its 12288-byte ceiling;"),
            "{err}"
        );
        write(dir.path(), "crates/demo/AGENTS.md", 10);
        assert_eq!(
            testutil::run(main, &[], &ctx),
            (0, String::new(), String::new())
        );
    }

    #[test]
    fn the_command_reads_the_root_option_and_refuses_unknown_arguments() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "repo/crates/demo/AGENTS.md",
            DEFAULT_CEILING + 1,
        );
        let ctx = Ctx::new(dir.path(), &[]);
        assert_eq!(testutil::run(main, &["--root", "repo"], &ctx).0, 1);
        assert_eq!(testutil::run(main, &["--root=repo"], &ctx).0, 1);
        assert_eq!(testutil::run(main, &[], &ctx).0, 0);
        let (code, _, err) = testutil::run(main, &["--bogus"], &ctx);
        assert_eq!(code, 2);
        assert!(
            err.contains("check_agents_md_size: error: unrecognized arguments: --bogus"),
            "{err}"
        );
    }
}
