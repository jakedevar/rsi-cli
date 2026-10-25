//! Refuse added lines that contain the operator's personal identifiers (#1454).
//!
//! Handoffs and Issue bodies sometimes quote the operator verbatim, and once a
//! commit reaches `rolling` and the GitHub mirror, agents cannot rewrite history.
//! This guard reads the operator's identifiers (e-mail, phone, name, address) from
//! a private local list and fails when a staged diff, or a commit range about to
//! be pushed, ADDS a line containing one. It scans `thoughts/` too, which is where
//! the first leak happened.
//!
//! The list is operator-owned local config, never committed:
//! `$RSI_OPERATOR_IDENTIFIERS_FILE`, else `~/.rsi/operator-identifiers`.
//! One identifier per line, matched case-insensitively as a literal substring;
//! blank lines and `#` comments are ignored. No file (or an empty one) means the
//! guard is a no-op, so clones without the list are unaffected.
//!
//! Findings never echo the identifier itself (that would re-leak it into logs and
//! commit output); they name the file, line number and list entry number.
//!
//! Usage: `rsi-repo-guard operator-identifiers --staged` (pre-commit) or
//! `--range A..B` (pre-push / lander). Exit 1 on any finding.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{self, Opt};
use crate::re::{Lazy, Re, group};
use crate::{Ctx, py};

static HUNK: Lazy = Lazy::new(|| Re::new(r"^@@ -\S+ \+(\d+)(?:,\d+)? @@"));

fn identifiers_path(ctx: &Ctx) -> PathBuf {
    match ctx.var("RSI_OPERATOR_IDENTIFIERS_FILE") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => ctx.home().join(".rsi").join("operator-identifiers"),
    }
}

/// The lower-cased identifiers of the list; `Err` when it is not UTF-8.
fn load_identifiers(path: &Path) -> Result<Vec<String>, String> {
    let text = match py::read_text(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(error.to_string());
        }
        Err(_) => return Ok(Vec::new()),
    };
    Ok(py::splitlines(&text)
        .into_iter()
        .map(py::strip)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_lowercase)
        .collect())
}

/// (path, line number, text) for every added line of a unified diff.
#[must_use]
pub fn added_lines(diff: &str) -> Vec<(String, usize, String)> {
    let mut found = Vec::new();
    let mut path: Option<String> = None;
    let mut lineno = 0_usize;
    for line in py::splitlines(diff) {
        if let Some(target) = line.strip_prefix("+++ ") {
            path = match target {
                "/dev/null" => None,
                t => Some(t.strip_prefix("b/").unwrap_or(t).to_string()),
            };
            continue;
        }
        if let Some(caps) = HUNK.search(line) {
            lineno = group(&caps, 1).parse().unwrap_or(0);
            continue;
        }
        let Some(current) = &path else { continue };
        if line.starts_with("--- ") || line.starts_with("diff ") || line.starts_with("index ") {
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            found.push((current.clone(), lineno, text.to_string()));
            lineno += 1;
        } else if !line.starts_with('-') {
            lineno += 1;
        }
    }
    found
}

/// (path, line number, 1-based list entry) per identifier on an added line.
#[must_use]
pub fn scan(diff: &str, identifiers: &[String]) -> Vec<(String, usize, usize)> {
    let mut findings = Vec::new();
    for (path, lineno, text) in added_lines(diff) {
        let low = text.to_lowercase();
        for (index, ident) in identifiers.iter().enumerate() {
            if low.contains(ident.as_str()) {
                findings.push((path.clone(), lineno, index + 1));
            }
        }
    }
    findings
}

/// Entry point: `operator-identifiers (--staged | --range A..B)`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let opts = [
        Opt {
            name: "--staged",
            value: false,
        },
        Opt {
            name: "--range",
            value: true,
        },
    ];
    let usage = "[-h] (--staged | --range REV_RANGE)";
    let parsed = match cli::parse(
        "check_operator_identifiers",
        usage,
        args,
        &opts,
        &[&["--staged", "--range"]],
        Some(0),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    if !parsed.flag("--staged") && parsed.value("--range").is_none() {
        let _ = writeln!(err, "usage: check_operator_identifiers {usage}");
        let _ = writeln!(
            err,
            "check_operator_identifiers: error: one of the arguments --staged --range is required"
        );
        return 2;
    }
    let path = identifiers_path(ctx);
    let identifiers = match load_identifiers(&ctx.path(&path)) {
        Ok(identifiers) => identifiers,
        Err(error) => {
            let _ = writeln!(err, "check_operator_identifiers: {error}");
            return 1;
        }
    };
    if identifiers.is_empty() {
        return 0;
    }
    let mut git = ctx.git();
    git.args(["diff", "--no-color", "--no-ext-diff", "--unified=0"]);
    match parsed.value("--range") {
        Some(range) => git.arg(range),
        None => git.arg("--cached"),
    };
    let diff = match git.output() {
        Ok(done) if done.status.success() => String::from_utf8_lossy(&done.stdout).into_owned(),
        // An unreadable range (e.g. remote tip not fetched) must not block work.
        _ => return 0,
    };
    let findings = scan(&diff, &identifiers);
    for (file, lineno, index) in &findings {
        let _ = writeln!(
            out,
            "{file}:{lineno}: operator identifier #{index} from the local identifiers list"
        );
    }
    if findings.is_empty() {
        return 0;
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "check-operator-identifiers: {} finding(s).",
        findings.len()
    );
    let _ = writeln!(
        out,
        "Do not commit operator personal data. Refer to it as \"the operator's"
    );
    let _ = writeln!(
        out,
        "<purpose> contact (local config)\" and keep the value out of git, Issue"
    );
    let _ = writeln!(
        out,
        "bodies and handoffs. Edit the list at {}",
        py::path_str(&py::as_posix(&path))
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git, run, write};

    // Fictional identifiers; the real list is private local config.
    fn email() -> String {
        ["jane.doe", "@example.invalid"].concat()
    }

    fn phone() -> String {
        ["555-0100-", "0199"].concat()
    }

    struct Repo {
        tmp: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let repo = Self { tmp };
            std::fs::create_dir(repo.dir()).unwrap();
            git(&repo.dir(), &["init", "-q"]);
            repo.write("README.md", "hello\n");
            git(&repo.dir(), &["add", "README.md"]);
            git(&repo.dir(), &["commit", "-qm", "init"]);
            repo
        }

        fn dir(&self) -> PathBuf {
            self.tmp.path().join("repo")
        }

        fn ids(&self) -> PathBuf {
            self.tmp.path().join("ids")
        }

        fn write(&self, rel: &str, text: &str) {
            write(&self.dir(), rel, text);
        }

        fn set_ids(&self, text: &str) {
            std::fs::write(self.ids(), text).unwrap();
        }

        fn run(&self, args: &[&str]) -> (i32, String) {
            let ids = self.ids();
            let ctx = Ctx::new(
                self.dir(),
                &[("RSI_OPERATOR_IDENTIFIERS_FILE", ids.to_str().unwrap())],
            );
            let (code, out, _) = run(main, args, &ctx);
            (code, out)
        }
    }

    #[test]
    fn staged_addition_in_thoughts_is_flagged_without_echoing_the_value() {
        let r = Repo::new();
        r.set_ids(&format!("# operator contacts\n{}\n", email()));
        r.write(
            "thoughts/shared/handoffs/h.md",
            &format!("line one\nContact: {}\n", email().to_uppercase()),
        );
        git(&r.dir(), &["add", "thoughts"]);
        let (code, out) = r.run(&["--staged"]);
        assert_eq!(code, 1);
        assert!(
            out.contains("thoughts/shared/handoffs/h.md:2: operator identifier #1"),
            "{out}"
        );
        assert!(!out.to_lowercase().contains(&email()));
    }

    #[test]
    fn clean_staged_diff_passes() {
        let r = Repo::new();
        r.set_ids(&format!("{}\n{}\n", email(), phone()));
        r.write("a.md", "the operator's release contact (local config)\n");
        git(&r.dir(), &["add", "a.md"]);
        assert_eq!(r.run(&["--staged"]), (0, String::new()));
    }

    #[test]
    fn range_flags_added_line_with_second_identifier() {
        let r = Repo::new();
        r.set_ids(&format!("{}\n{}\n", email(), phone()));
        r.write("a.rs", &format!("const P: &str = \"{}\";\n", phone()));
        git(&r.dir(), &["add", "a.rs"]);
        git(&r.dir(), &["commit", "-qm", "leak"]);
        let (code, out) = r.run(&["--range", "HEAD~1..HEAD"]);
        assert_eq!(code, 1);
        assert!(out.contains("a.rs:1: operator identifier #2"), "{out}");
    }

    #[test]
    fn pre_existing_lines_and_removals_are_not_flagged() {
        let r = Repo::new();
        r.write("a.md", &format!("old {}\n", email()));
        git(&r.dir(), &["add", "a.md"]);
        git(&r.dir(), &["commit", "-qm", "history"]);
        r.set_ids(&format!("{}\n", email()));
        r.write("a.md", "scrubbed\n");
        git(&r.dir(), &["add", "a.md"]);
        assert_eq!(r.run(&["--staged"]), (0, String::new()));
    }

    #[test]
    fn missing_or_empty_list_is_a_noop() {
        let r = Repo::new();
        r.write("a.md", &format!("{}\n", email()));
        git(&r.dir(), &["add", "a.md"]);
        assert_eq!(r.run(&["--staged"]), (0, String::new())); // no list file at all
        r.set_ids("# only comments\n\n");
        assert_eq!(r.run(&["--staged"]), (0, String::new()));
    }

    #[test]
    fn unreadable_range_does_not_block() {
        let r = Repo::new();
        r.set_ids(&format!("{}\n", email()));
        assert_eq!(
            r.run(&["--range", "deadbeef..cafebabe"]),
            (0, String::new())
        );
    }

    #[test]
    fn a_mode_is_required_and_the_modes_exclude_each_other() {
        let r = Repo::new();
        let ctx = Ctx::new(r.dir(), &[]);
        let (code, _, err) = run(main, &[], &ctx);
        assert_eq!(code, 2);
        assert!(
            err.contains("one of the arguments --staged --range is required"),
            "{err}"
        );
        let (code, _, err) = run(main, &["--staged", "--range", "A..B"], &ctx);
        assert_eq!(code, 2);
        assert!(err.contains("not allowed with argument --staged"), "{err}");
    }

    #[test]
    fn the_summary_names_the_list_without_its_values() {
        let r = Repo::new();
        r.set_ids(&format!("{}\n", email()));
        r.write("a.md", &format!("{}\n", email()));
        git(&r.dir(), &["add", "a.md"]);
        let (code, out) = r.run(&["--staged"]);
        assert_eq!(code, 1);
        assert!(
            out.contains("\ncheck-operator-identifiers: 1 finding(s).\n"),
            "{out}"
        );
        assert!(
            out.trim_end()
                .ends_with(&format!("Edit the list at {}", py::as_posix(&r.ids()))),
            "{out}"
        );
    }
}
