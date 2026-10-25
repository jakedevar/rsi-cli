//! Refuse Rust includes of paths the public rsi-cli export excludes (#1630).
//!
//! `scripts/export-rsi-cli.sh` removes the `EXCLUDES` paths from the exported
//! tree, so an `include_str!`/`include_bytes!`/`include!`/`#[path]` that reaches
//! into one breaks the public build. The exclusion list is read from the export
//! script itself.
//!
//! Usage: `rsi-repo-guard export-includes [--root DIR]` (exit 1 on any finding).

use std::io::Write;
use std::path::Path;

use crate::re::{Lazy, Re, group, start};
use crate::{Ctx, py};

static INCLUDE: Lazy = Lazy::new(|| {
    Re::new(r#"(?:\b(?:include_str|include_bytes|include)!\s*\(\s*|#\[\s*path\s*=\s*)"([^"]+)""#)
});
static EXCLUDES: Lazy = Lazy::new(|| Re::new(r"(?sm)^EXCLUDES=\(\n(.*?)^\)"));

/// The `EXCLUDES` array of `scripts/export-rsi-cli.sh` under `root`.
pub fn read_excludes(root: &Path) -> Result<Vec<String>, String> {
    let path = root.join("scripts").join("export-rsi-cli.sh");
    let text = py::read_text(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let Some(caps) = EXCLUDES.search(&text) else {
        return Err("check_export_includes: EXCLUDES array not found".to_string());
    };
    Ok(py::splitlines(group(&caps, 1))
        .into_iter()
        .filter(|line| !py::strip(line).is_empty())
        .map(|line| py::strip(line).trim_end_matches('/').to_string())
        .collect())
}

fn is_excluded(rel: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|e| {
        rel == e
            || rel
                .strip_prefix(e.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Findings for the repository-relative `files` under `root`.
#[must_use]
pub fn scan(root: &Path, files: &[String], excludes: &[String]) -> Vec<String> {
    let mut findings = Vec::new();
    for file in files {
        if !file.ends_with(".rs") || is_excluded(file, excludes) {
            continue;
        }
        let Ok(text) = py::read_text(&root.join(file)) else {
            continue;
        };
        let dir = file.rsplit_once('/').map_or("", |(dir, _)| dir);
        for caps in INCLUDE.find_all(&text) {
            let target = group(&caps, 1);
            let joined = if target.starts_with('/') || dir.is_empty() {
                target.to_string()
            } else {
                format!("{dir}/{target}")
            };
            let target = py::normpath(&joined);
            if is_excluded(&target, excludes) {
                let line = py::count_newlines(&text, start(&caps)) + 1;
                findings.push(format!("{file}:{line}: includes excluded path {target}"));
            }
        }
    }
    findings
}

/// Entry point: `export-includes [--root DIR]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let root = if args.first().map(String::as_str) == Some("--root") {
        let Some(root) = args.get(1) else {
            let _ = writeln!(err, "check_export_includes: --root needs a directory");
            return 1;
        };
        ctx.path(root)
    } else {
        match ctx.git().args(["rev-parse", "--show-toplevel"]).output() {
            Ok(done) if done.status.success() => {
                ctx.path(py::strip(&String::from_utf8_lossy(&done.stdout)))
            }
            _ => {
                let _ = writeln!(err, "check_export_includes: not inside a git repository");
                return 1;
            }
        }
    };
    let listed = match ctx.git().arg("-C").arg(&root).arg("ls-files").output() {
        Ok(done) if done.status.success() => {
            py::universal_newlines(&String::from_utf8_lossy(&done.stdout))
        }
        _ => {
            let _ = writeln!(
                err,
                "check_export_includes: git ls-files failed in {}",
                root.display()
            );
            return 1;
        }
    };
    let files: Vec<String> = py::splitlines(&listed)
        .into_iter()
        .map(str::to_string)
        .collect();
    let excludes = match read_excludes(&root) {
        Ok(excludes) => excludes,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return 1;
        }
    };
    let findings = scan(&root, &files, &excludes);
    for line in &findings {
        let _ = writeln!(out, "{line}");
    }
    if findings.is_empty() {
        return 0;
    }
    let _ = writeln!(
        out,
        "export-rsi-cli.sh excludes these paths; move the file to a public path."
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write;

    const SCRIPT: &str = "EXCLUDES=(\n    thoughts\n    docs/design\n)\n";

    // Fixture sources use `q()` quotes so this file holds no include the guard reads.
    fn q(text: &str) -> String {
        text.replace('`', "\"")
    }

    fn scan_files(files: &[(&str, &str)]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "scripts/export-rsi-cli.sh", SCRIPT);
        for (name, text) in files {
            write(dir.path(), name, &q(text));
        }
        let names: Vec<String> = files.iter().map(|(name, _)| (*name).to_string()).collect();
        scan(dir.path(), &names, &read_excludes(dir.path()).unwrap())
    }

    #[test]
    fn reads_excludes_from_export_script() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "scripts/export-rsi-cli.sh", SCRIPT);
        assert_eq!(
            read_excludes(dir.path()).unwrap(),
            vec!["thoughts", "docs/design"]
        );
    }

    #[test]
    fn include_str_of_excluded_dir_is_flagged() {
        let out = scan_files(&[(
            "crates/a/src/x.rs",
            "let s = include_str!(`../../../thoughts/n.md`);\n",
        )]);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("thoughts/n.md"), "{out:?}");
    }

    #[test]
    fn path_attribute_and_include_bytes_are_flagged() {
        let out = scan_files(&[(
            "crates/a/src/x.rs",
            "#[path = `../../../docs/design/t.rs`]\nmod t;\nconst B: &[u8] = include_bytes!(`../../../thoughts/b.bin`);\n",
        )]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(
            out[0],
            "crates/a/src/x.rs:1: includes excluded path docs/design/t.rs"
        );
    }

    #[test]
    fn public_include_is_accepted() {
        assert!(
            scan_files(&[(
                "crates/a/src/x.rs",
                "include_str!(`../../../docs/keybindings.md`);\n"
            )])
            .is_empty()
        );
    }

    #[test]
    fn the_repository_passes() {
        let root = crate::testutil::repo_root();
        let ctx = Ctx::new(&root, &[]);
        let (code, out, err) =
            crate::testutil::run(main, &["--root", root.to_str().unwrap()], &ctx);
        assert_eq!(code, 0, "{out}{err}");
    }
}
