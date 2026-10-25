//! Flag tests asserting that an entity's own identity string is absent from the UI.
//!
//! See `scripts/check-identity-assertions.sh` for rationale. Narrow by
//! construction: a literal is only considered "identity" if the same file
//! assigns it to an identity-bearing field.
//!
//! Usage: `rsi-repo-guard identity-assertions [paths...]` (default `crates`); exit 1 on any finding.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use crate::re::{Lazy, Re, group};
use crate::{Ctx, py};

/// `x.title = Some("Lit")`, `title: Some("Lit")`, `.title = "Lit"`, `title: "Lit"`.
static ASSIGN: Lazy = Lazy::new(|| {
    Re::new(
        r#"\.?\b(?:title|name|query|display_title|short_summary)\b\s*[:=]\s*(?:Some\(\s*)?"((?:[^"\\]|\\.)+)""#,
    )
});
/// `!<expr>.contains("Lit")`: the negation is what makes it an absence assertion.
static ABSENCE: Lazy = Lazy::new(|| {
    Re::new(r#"(?<![A-Za-z0-9_])!\s*[A-Za-z0-9_\.\[\]&\*]*\.contains\(\s*"((?:[^"\\]|\\.)+)""#)
});

/// (line number, needle, stripped line) per absence assertion of an assigned identity.
#[must_use]
pub fn scan_text(text: &str) -> Vec<(usize, String, String)> {
    let identities: HashSet<&str> = ASSIGN
        .find_all(text)
        .iter()
        .map(|caps| group(caps, 1))
        .collect();
    if identities.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (index, line) in py::splitlines(text).into_iter().enumerate() {
        for caps in ABSENCE.find_all(line) {
            let needle = group(&caps, 1);
            if identities.contains(needle) {
                out.push((index + 1, needle.to_string(), py::strip(line).to_string()));
            }
        }
    }
    out
}

fn scan(path: &Path) -> Vec<(usize, String, String)> {
    py::read_text_lossy(path).map_or_else(|_| Vec::new(), |text| scan_text(&text))
}

/// The files to scan with their display names, in the Python guard's order.
fn files(args: &[String], ctx: &Ctx) -> Vec<(String, std::path::PathBuf)> {
    let roots: Vec<String> = if args.is_empty() {
        vec!["crates".to_string()]
    } else {
        args.to_vec()
    };
    let mut files = Vec::new();
    for root in roots {
        let shown = py::path_str(&root);
        let path = ctx.path(&root);
        if path.is_file() {
            files.push((shown, path));
            continue;
        }
        for found in py::rglob(&path, &|name| name.ends_with(".rs")) {
            let rel = py::as_posix(found.strip_prefix(&path).unwrap_or(&found));
            files.push((join_display(&shown, &rel), found));
        }
    }
    files
}

/// `str(Path(shown) / rel)`.
pub(crate) fn join_display(shown: &str, rel: &str) -> String {
    match shown {
        "." => rel.to_string(),
        s if s.ends_with('/') => format!("{s}{rel}"),
        s => format!("{s}/{rel}"),
    }
}

/// Entry point: `identity-assertions [paths...]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, _err: &mut dyn Write) -> i32 {
    let mut findings = 0;
    for (shown, path) in files(args, ctx) {
        for (lineno, needle, line) in scan(&path) {
            findings += 1;
            let _ = writeln!(
                out,
                "{shown}:{lineno}: asserts an entity's own identity is absent: {}",
                py::repr(&needle)
            );
            let _ = writeln!(out, "    {line}");
        }
    }
    if findings == 0 {
        return 0;
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "check-identity-assertions: {findings} finding(s).");
    let _ = writeln!(
        out,
        "A test must not require an entity's own name to be invisible."
    );
    let _ = writeln!(
        out,
        "Assert the positive end state instead (what the user SHOULD see)."
    );
    let _ = writeln!(
        out,
        "If the omission is genuinely correct, assert the specific"
    );
    let _ = writeln!(
        out,
        "replacement is present and name the test so the rationale is explicit."
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{run, write};

    // Fixtures are built with `q()` so this file holds no assignment and absence
    // pair the guard would itself flag when it scans `crates/`.
    fn q(text: &str) -> String {
        text.replace('`', "\"")
    }

    #[test]
    fn an_assigned_title_asserted_absent_is_flagged() {
        let text =
            q("let g = Group { title: `Alpha`.into() };\nassert!(!out.contains(`Alpha`));\n");
        assert_eq!(
            scan_text(&text),
            vec![(
                2,
                "Alpha".to_string(),
                q("assert!(!out.contains(`Alpha`));")
            )]
        );
        let text = q("s.name = Some(`Beta`);\nassert!( ! rendered[0].contains( `Beta` ));\n");
        assert_eq!(scan_text(&text).len(), 1);
    }

    #[test]
    fn chrome_and_positive_assertions_are_not_flagged() {
        assert!(
            scan_text(&q(
                "let g = Group { title: `Alpha` };\nassert!(out.contains(`Alpha`));\n"
            ))
            .is_empty()
        );
        assert!(scan_text(&q("assert!(!out.contains(`[Enter] open`));\n")).is_empty());
        assert!(
            scan_text(&q(
                "let g = Group { title: `Alpha` };\nassert!(x!out.contains(`Alpha`));\n"
            ))
            .is_empty()
        );
        assert!(
            scan_text(&q(
                "let label = `Alpha`;\nassert!(!out.contains(`Alpha`));\n"
            ))
            .is_empty()
        );
    }

    #[test]
    fn the_cli_prints_findings_and_the_rationale() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "crates/a/src/lib.rs",
            &q("x.title = `It's`;\nassert!(!v.contains(`It's`));\n"),
        );
        write(dir.path(), "crates/a/src/clean.rs", "fn f() {}\n");
        let ctx = Ctx::new(dir.path(), &[]);
        let (code, out, _) = run(main, &[], &ctx);
        assert_eq!(code, 1, "{out}");
        assert!(out.starts_with(&q(
            "crates/a/src/lib.rs:2: asserts an entity's own identity is absent: `It's`\n    assert!(!v.contains(`It's`));\n"
        )));
        assert!(out.contains("check-identity-assertions: 1 finding(s)."));
        assert_eq!(
            run(main, &["crates/a/src/clean.rs"], &ctx),
            (0, String::new(), String::new())
        );
        let (code, out, _) = run(main, &["./crates/a/src/lib.rs"], &ctx);
        assert_eq!(code, 1);
        assert!(out.starts_with("crates/a/src/lib.rs:2:"));
    }

    #[test]
    fn the_repository_crates_pass() {
        let ctx = Ctx::new(crate::testutil::repo_root(), &[]);
        let (code, out, _) = run(main, &["crates"], &ctx);
        assert_eq!(code, 0, "{out}");
    }
}
