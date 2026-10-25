//! Flag unguarded empty-array expansions under `set -u` (macOS /bin/bash 3.2).
//!
//! bash < 4.4 treats `"${arr[@]}"` / `"${arr[*]}"` of an empty array as an unbound
//! variable under `set -u`. A script that runs on the macOS satellite must expand a
//! possibly-empty array as `${arr[@]+"${arr[@]}"}` (or guard on `${#arr[@]}`).
//!
//! An array is "possibly empty" when the file declares it empty (`arr=()`,
//! `local -a arr`, `declare -a arr`) or fills it with `mapfile`/`read -a`. An array
//! whose only declaration is a non-empty literal (`arr=(a b)`, then `+=`) is safe.
//! An expansion is allowed when it uses the `+` guard form, or carries
//! `# bash32-safe` on its line or the line above (use it when a preceding length
//! check proves the array is non-empty).
//!
//! Linux-only scripts (systemd/journalctl hosts such as satellite/install-direct.sh)
//! are not in the audited set.
//!
//! Usage: `rsi-repo-guard bash32-arrays [paths...]` (default: the audited macOS set,
//! relative to the repository root). Exit 1 and name file:line:array per finding.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::re::{Lazy, Re, group, start};
use crate::{Ctx, py};

pub const AUDITED: &[&str] = &[
    "scripts/rsi-stash",
    "scripts/fmt-changed.sh",
    "scripts/cargo-slot",
    "scripts/cargo-slot-wrapper",
    "scripts/rsid-launchd.sh",
    "scripts/rsid-supervisor.sh",
    "scripts/install-release.sh",
    "scripts/release-target-guard.sh",
    "scripts/clean-target",
    "scripts/rsi-scratch-mkdir",
    "scripts/e2e.sh",
    "scripts/sync-agent-commands.sh",
    "scripts/render-project-index.sh",
    "scripts/rsi-ssh-tui.sh",
    "scripts/cleanup",
    "scripts/ios",
    "scripts/satellite/dial-home.sh",
    "scripts/satellite/direct.sh",
    "scripts/satellite/install.sh",
];
static SET_U: Lazy = Lazy::new(|| Re::new(r"(?m)^\s*set\s+-[A-Za-z]*u|^\s*set\s+-o\s+nounset"));
static EXPAND: Lazy = Lazy::new(|| Re::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\[([@*])\]\}"));
static ASSIGN_LITERAL: Lazy = Lazy::new(|| {
    Re::new(
        r"^\s*(?:local\s+|declare\s+(?:-[a-z]+\s+)?|readonly\s+)?([A-Za-z_][A-Za-z0-9_]*)=\((.*)",
    )
});
static DECL_ONLY: Lazy = Lazy::new(|| {
    Re::new(r"^\s*(?:local|declare|typeset)\s+-a\s+([A-Za-z_][A-Za-z0-9_ ]*)\s*(?:#.*)?$")
});
static FILL_CMD: Lazy = Lazy::new(|| {
    Re::new(concat!(
        r"\b(?:mapfile|readarray)\b[^\n]*?\s([A-Za-z_][A-Za-z0-9_]*)\s*(?:<|$)",
        r"|\bread\s+(?:-[a-zA-Z]+\s+)*-a\s+([A-Za-z_][A-Za-z0-9_]*)",
    ))
});
const MARKER: &str = "bash32-safe";

fn possibly_empty_arrays(lines: &[&str]) -> HashSet<String> {
    let mut empty = HashSet::new();
    for raw in lines {
        let line = raw.split('#').next().unwrap_or("");
        if let Some(caps) = ASSIGN_LITERAL.search(line)
            && py::strip(group(&caps, 2)).starts_with(')')
        {
            empty.insert(group(&caps, 1).to_string());
        }
        if let Some(caps) = DECL_ONLY.search(line) {
            empty.extend(py::split_whitespace(group(&caps, 1)).map(str::to_string));
        }
        for caps in FILL_CMD.find_all(line) {
            let name = if caps.get(1).is_some() {
                group(&caps, 1)
            } else {
                group(&caps, 2)
            };
            empty.insert(name.to_string());
        }
    }
    empty
}

/// (name, line, array, stripped line) per unguarded expansion of a possibly-empty array.
#[must_use]
pub fn scan_text(text: &str, name: &str) -> Vec<(String, usize, String, String)> {
    if !SET_U.is_match(text) {
        return Vec::new();
    }
    let lines = py::splitlines(text);
    let empty = possibly_empty_arrays(&lines);
    let mut findings = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        if py::lstrip(raw).starts_with('#') {
            continue;
        }
        let previous = if index > 0 { lines[index - 1] } else { "" };
        if raw.contains(MARKER) || previous.contains(MARKER) {
            continue;
        }
        for caps in EXPAND.find_all(raw) {
            let array = group(&caps, 1);
            if !empty.contains(array) {
                continue;
            }
            // `${arr[@]+"${arr[@]}"}`: the expansion we found is the inner one.
            let before = &raw[..start(&caps)];
            if before.ends_with(&format!("${{{array}[@]+\""))
                || before.ends_with(&format!("${{{array}[@]+"))
            {
                continue;
            }
            findings.push((
                name.to_string(),
                index + 1,
                array.to_string(),
                py::strip(raw).to_string(),
            ));
        }
    }
    findings
}

/// Every file the given items name (a directory: its files, sorted), relative to `root`.
#[must_use]
pub fn expand_paths(root: &Path, items: &[String]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for item in items {
        let path = root.join(item);
        if path.is_dir() {
            found.extend(py::rglob(&path, &|_| true));
        } else if path.is_file() {
            found.push(path);
        }
    }
    found
}

fn is_bash(path: &Path) -> bool {
    let Ok(text) = py::read_text_lossy(path) else {
        return false;
    };
    py::splitlines(&text)
        .first()
        .is_some_and(|head| head.starts_with("#!") && head.contains("bash"))
}

/// The repository root for relative paths: the git top level of the working
/// directory, else the working directory itself.
fn root(ctx: &Ctx) -> PathBuf {
    match ctx.git().args(["rev-parse", "--show-toplevel"]).output() {
        Ok(done) if done.status.success() => {
            PathBuf::from(py::strip(&String::from_utf8_lossy(&done.stdout)))
        }
        _ => ctx.cwd.clone(),
    }
}

/// Entry point: `bash32-arrays [paths...]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, _err: &mut dyn Write) -> i32 {
    let root = root(ctx);
    let targets: Vec<String> = if args.is_empty() {
        AUDITED.iter().map(|item| (*item).to_string()).collect()
    } else {
        args.to_vec()
    };
    let mut findings = Vec::new();
    for path in expand_paths(&root, &targets) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if py::suffix(&name) != ".sh" && !is_bash(&path) {
            continue;
        }
        let shown = path
            .strip_prefix(&root)
            .map_or_else(|_| py::as_posix(&path), py::as_posix);
        let text = py::read_text_lossy(&path).unwrap_or_default();
        findings.extend(scan_text(&text, &shown));
    }
    for (name, number, array, line) in &findings {
        let _ = writeln!(
            out,
            "{name}:{number}: unguarded empty-array expansion of '{array}' under set -u (bash 3.2): \
             use ${{{array}[@]+\"${{{array}[@]}}\"}} or mark '# {MARKER}'\n    {line}"
        );
    }
    i32::from(!findings.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flagged(source: &str) -> Vec<String> {
        scan_text(source, "<text>")
            .into_iter()
            .map(|finding| finding.2)
            .collect()
    }

    #[test]
    fn flags_unguarded_expansion_of_empty_array() {
        assert_eq!(
            flagged("set -u\na=()\nfor x in \"${a[@]}\"; do :; done\n"),
            vec!["a"]
        );
        assert_eq!(
            flagged("set -euo pipefail\nlocal -a a\nf \"${a[*]}\"\n"),
            vec!["a"]
        );
        assert_eq!(
            flagged("set -eu\nmapfile -t a < f\nf \"${a[@]}\"\n"),
            vec!["a"]
        );
    }

    #[test]
    fn accepts_guard_forms_and_nonempty_literals() {
        for source in [
            "set -u\na=()\nf ${a[@]+\"${a[@]}\"}\n",
            "set -u\na=(x y)\na+=(z)\nf \"${a[@]}\"\n",
            "set -u\na=()\n(( ${#a[@]} )) && f \"${#a[@]}\"\n",
            "set -u\na=()\n# bash32-safe: non-empty here\nf \"${a[@]}\"\n",
            "a=()\nf \"${a[@]}\"\n", // no set -u
            "# f \"${a[@]}\"\nset -u\na=()\n",
        ] {
            assert_eq!(flagged(source), Vec::<String>::new(), "{source}");
        }
    }

    #[test]
    fn audited_macos_scripts_are_clean() {
        let root = crate::testutil::repo_root();
        let items: Vec<String> = AUDITED.iter().map(|item| (*item).to_string()).collect();
        let mut findings = Vec::new();
        for path in expand_paths(&root, &items) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if py::suffix(&name) == ".sh" || is_bash(&path) {
                let rel = py::as_posix(path.strip_prefix(&root).unwrap());
                findings.extend(scan_text(&py::read_text_lossy(&path).unwrap(), &rel));
            }
        }
        assert_eq!(findings, Vec::new());
    }

    /// (major, minor) of a bash binary, if it runs.
    fn bash_version(path: &str) -> Option<(u32, u32)> {
        let done = std::process::Command::new(path)
            .args(["-c", "echo \"${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}\""])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&done.stdout);
        let (major, minor) = text.trim().split_once('.')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    }

    #[test]
    fn guard_idiom_is_legal_on_bash32_and_bare_form_is_not() {
        let candidates = [
            "/bin/bash",
            "/usr/bin/bash",
            "/opt/homebrew/bin/bash",
            "/usr/local/bin/bash",
        ];
        let Some(bash32) = candidates.iter().find(|path| {
            Path::new(path).exists() && bash_version(path).is_some_and(|v| v < (4, 4))
        }) else {
            return; // no bash < 4.4 on this host (macOS /bin/bash 3.2 is the target)
        };
        let run = |script: &str| {
            std::process::Command::new(bash32)
                .args(["-c", script])
                .output()
                .unwrap()
        };
        let guarded = run("set -u; a=(); printf \"%s\" ${a[@]+\"${a[@]}\"}");
        assert!(
            guarded.status.success(),
            "{}",
            String::from_utf8_lossy(&guarded.stderr)
        );
        assert!(
            !run("set -u; a=(); printf \"%s\" \"${a[@]}\"")
                .status
                .success()
        );
    }

    #[test]
    fn cli_reports_a_planted_violation() {
        let dir = tempfile::tempdir().unwrap();
        let script = crate::testutil::write(
            dir.path(),
            "bad.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\nargs=()\nexec cmd \"${args[@]}\"\n",
        );
        let ctx = Ctx::new(dir.path(), &[]);
        let (code, out, _) = crate::testutil::run(main, &[script.to_str().unwrap()], &ctx);
        assert_eq!(code, 1);
        assert!(
            out.contains("bad.sh:4: unguarded empty-array expansion of 'args'"),
            "{out}"
        );
    }
}
