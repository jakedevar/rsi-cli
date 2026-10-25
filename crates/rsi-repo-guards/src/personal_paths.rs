//! Fail on personal home paths / account names and public IP literals (#730, #3269).
//!
//! See `scripts/check-personal-paths.sh` for rationale and usage. Scans tracked
//! files (or the paths given), skipping `thoughts/` (historical records).
//!
//! `--staged` (the pre-commit mode) judges only the lines the staged diff adds, so
//! an edit is not blocked by matches that were already committed in the same file.
//! During a merge, only lines added relative to every parent are judged.
//!
//! Usage: `rsi-repo-guard personal-paths [--staged | paths...]`; exit 1 on any finding.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use crate::re::{Lazy, Re, group};
use crate::{Ctx, py};

/// Built by concatenation so this file does not trip its own scan.
static PERSONAL: Lazy = Lazy::new(|| Re::new(concat!("jake", "devar|jacob", "devarennes")));

// The whole allowlist. Each entry is deliberate and small:
//  * a GitHub repository slug is public project identity, not a home path;
static ALLOWED_LINE: Lazy = Lazy::new(|| Re::new(concat!(r"github\.com/jake", "devar/")));
//  * well-known public resolver used as a "not a tailnet address" fixture.
const ALLOWED_PUBLIC_IPS: &[&str] = &["8.8.8.8"];
//  * the egress classifier (#774) must name real public and range-boundary
//    addresses as test inputs; personal-path checks still apply there.
const IP_FIXTURE_PATHS: &[&str] = &[
    "crates/rsi-common/src/egress_policy.rs",
    "crates/rsid/src/session/harness/egress.rs",
];

const SKIP_PREFIXES: &[&str] = &["thoughts/"];
const SKIP_NAMES: &[&str] = &["Cargo.lock"];
const SKIP_SUFFIXES: &[&str] = &[
    ".lock", ".svg", ".png", ".jpg", ".gif", ".ico", ".pdf", ".woff", ".woff2",
];

static QUAD: Lazy =
    Lazy::new(|| Re::new(r"(?<![0-9.])([0-9]{1,3}(?:\.[0-9]{1,3}){3})(?![0-9])(?!\.[0-9])"));
static HUNK: Lazy = Lazy::new(|| Re::new(r"^@@ -\S+ \+(\d+)(?:,\d+)? @@"));

fn skipped(path: &str) -> bool {
    let name = py::path_str(path);
    let name = py::basename(&name);
    SKIP_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
        || SKIP_NAMES.contains(&name)
        || SKIP_SUFFIXES.iter().any(|suffix| path.ends_with(suffix))
}

/// `ipaddress.IPv4Address(text)`: four decimal octets, no leading zeros.
fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0_u8; 4];
    for (octet, part) in octets.iter_mut().zip(parts) {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        *octet = part.parse().ok()?;
    }
    Some(octets)
}

fn in_net(ip: [u8; 4], net: [u8; 4], prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (u32::from_be_bytes(ip) & mask) == (u32::from_be_bytes(net) & mask)
}

/// Python 3.13 `ipaddress` `is_private` for IPv4.
fn is_private(ip: [u8; 4]) -> bool {
    const PRIVATE: &[([u8; 4], u32)] = &[
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 24),
        ([192, 0, 0, 170], 31),
        ([192, 0, 2, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        ([240, 0, 0, 0], 4),
        ([255, 255, 255, 255], 32),
    ];
    const EXCEPTIONS: &[([u8; 4], u32)] = &[([192, 0, 0, 9], 32), ([192, 0, 0, 10], 32)];
    PRIVATE
        .iter()
        .any(|(net, prefix)| in_net(ip, *net, *prefix))
        && !EXCEPTIONS
            .iter()
            .any(|(net, prefix)| in_net(ip, *net, *prefix))
}

/// A globally routable unicast host literal that is not on the allowlist.
fn public_ip(text: &str) -> bool {
    let Some(ip) = parse_ipv4(text) else {
        return false;
    };
    // The guard is about globally routable unicast hosts. Multicast (224.0.0.0/4)
    // and the reserved block (240.0.0.0/4) are not hosts, even where
    // ipaddress.is_global says otherwise (it reports 224/4 as global).
    if in_net(ip, [224, 0, 0, 0], 4) || in_net(ip, [240, 0, 0, 0], 4) {
        return false;
    }
    let global = !in_net(ip, [100, 64, 0, 0], 10) && !is_private(ip);
    global && !ALLOWED_PUBLIC_IPS.contains(&text)
}

/// The finding messages for one line of text.
#[must_use]
pub fn scan_line(line: &str, check_ips: bool) -> Vec<String> {
    let mut out = Vec::new();
    let stripped = ALLOWED_LINE.sub(line, "");
    if PERSONAL.is_match(&stripped) {
        out.push("personal path or account name".to_string());
    }
    if check_ips {
        for caps in QUAD.find_all(line) {
            if public_ip(group(&caps, 1)) {
                out.push(format!("public IP literal {}", group(&caps, 1)));
            }
        }
    }
    out
}

fn scan(path: &Path, check_ips: bool) -> Vec<(usize, String)> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    if data.contains(&0) {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&data);
    let mut out = Vec::new();
    for (index, line) in py::splitlines(&text).into_iter().enumerate() {
        out.extend(
            scan_line(line, check_ips)
                .into_iter()
                .map(|what| (index + 1, what)),
        );
    }
    out
}

/// (path, line number, text) for every added line of a unified diff.
#[must_use]
pub fn added_lines(diff: &str) -> Vec<(String, usize, String)> {
    let mut found = Vec::new();
    let mut path: Option<String> = None;
    let mut lineno = 0_usize;
    let mut in_hunk = false;
    for line in py::splitlines(diff) {
        if line.starts_with("diff ") {
            in_hunk = false;
            continue;
        }
        if !in_hunk && let Some(target) = line.strip_prefix("+++ ") {
            path = match target {
                "/dev/null" => None,
                t => Some(t.strip_prefix("b/").unwrap_or(t).to_string()),
            };
            continue;
        }
        if let Some(caps) = HUNK.search(line) {
            lineno = group(&caps, 1).parse().unwrap_or(0);
            in_hunk = true;
            continue;
        }
        let Some(current) = &path else { continue };
        if !in_hunk {
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

/// Findings (path, line number, what) for the added lines of a unified diff.
#[must_use]
pub fn scan_diff(diff: &str) -> Vec<(String, usize, String)> {
    scan_added_lines(added_lines(diff))
}

fn scan_added_lines(lines: Vec<(String, usize, String)>) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    for (path, lineno, text) in lines {
        if skipped(&path) {
            continue;
        }
        for what in scan_line(&text, !IP_FIXTURE_PATHS.contains(&path.as_str())) {
            out.push((path.clone(), lineno, what));
        }
    }
    out
}

fn staged_diff(ctx: &Ctx, parent: Option<&str>) -> Result<String, String> {
    let mut git = ctx.git();
    git.args([
        "-c",
        "core.quotepath=off",
        "diff",
        "--cached",
        "--no-color",
        "--no-ext-diff",
        "--unified=0",
    ]);
    git.arg("--diff-filter=d");
    if let Some(parent) = parent {
        git.args([parent, "--"]);
    }
    match git.output() {
        Ok(done) if done.status.success() => Ok(String::from_utf8_lossy(&done.stdout).into_owned()),
        Ok(done) => Err(format!(
            "git diff --cached failed: {}",
            py::strip(&String::from_utf8_lossy(&done.stderr))
        )),
        Err(error) => Err(format!("git diff --cached failed: {error}")),
    }
}

fn staged_lines(ctx: &Ctx) -> Result<Vec<(String, usize, String)>, String> {
    let mut lines = added_lines(&staged_diff(ctx, None)?);
    let merge_head = match ctx
        .git()
        .args(["rev-parse", "--git-path", "MERGE_HEAD"])
        .output()
    {
        Ok(done) if done.status.success() => {
            py::strip(&String::from_utf8_lossy(&done.stdout)).to_string()
        }
        _ => return Err("git rev-parse --git-path MERGE_HEAD failed".to_string()),
    };
    let parents = match std::fs::read_to_string(ctx.path(&merge_head)) {
        Ok(text) => py::universal_newlines(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(lines),
        Err(error) => return Err(format!("{merge_head}: {error}")),
    };
    // Each diff uses the same index as its target, so target path + line number
    // identifies the same staged line even when its position differs in a parent.
    // MERGE_HEAD can hold multiple parents (an octopus merge).
    for parent in py::splitlines(&parents) {
        let added: HashSet<(String, usize)> = added_lines(&staged_diff(ctx, Some(parent))?)
            .into_iter()
            .map(|(path, lineno, _)| (path, lineno))
            .collect();
        lines.retain(|(path, lineno, _)| added.contains(&(path.clone(), *lineno)));
    }
    Ok(lines)
}

fn tracked_files(ctx: &Ctx) -> Result<Vec<String>, String> {
    match ctx.git().args(["ls-files", "-z"]).output() {
        Ok(done) if done.status.success() => Ok(String::from_utf8_lossy(&done.stdout)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect()),
        _ => Err("git ls-files failed".to_string()),
    }
}

/// Entry point: `personal-paths [--staged | paths...]`.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut findings = 0;
    if args.first().map(String::as_str) == Some("--staged") {
        let lines = match staged_lines(ctx) {
            Ok(lines) => lines,
            Err(error) => {
                let _ = writeln!(err, "check_personal_paths: {error}");
                return 1;
            }
        };
        for (path, lineno, what) in scan_added_lines(lines) {
            findings += 1;
            let _ = writeln!(out, "{path}:{lineno}: {what}");
        }
    } else {
        let paths = if args.is_empty() {
            match tracked_files(ctx) {
                Ok(paths) => paths,
                Err(error) => {
                    let _ = writeln!(err, "check_personal_paths: {error}");
                    return 1;
                }
            }
        } else {
            args.to_vec()
        };
        for path in &paths {
            let norm = path.strip_prefix("./").unwrap_or(path);
            if skipped(norm) {
                continue;
            }
            for (lineno, what) in scan(&ctx.path(path), !IP_FIXTURE_PATHS.contains(&norm)) {
                findings += 1;
                let _ = writeln!(out, "{path}:{lineno}: {what}");
            }
        }
    }
    if findings == 0 {
        return 0;
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "check-personal-paths: {findings} finding(s).");
    let _ = writeln!(
        out,
        "Use $HOME, ~/, <repo>-relative paths, or a placeholder host such as"
    );
    let _ = writeln!(
        out,
        "remote.example.net / 203.0.113.10. Historical records belong under thoughts/."
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git, git_status, write};

    fn account() -> String {
        ["jake", "devar"].concat()
    }

    // Assembled at runtime so this file holds no public IP literal itself.
    fn public_ip_text() -> String {
        ["93", "184", "216", "34"].join(".")
    }

    fn other_resolver() -> String {
        ["8", "8", "4", "4"].join(".")
    }

    fn run_file(name: &str, text: &str) -> (i32, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), name, text);
        let (code, out, _) =
            crate::testutil::run(main, &[path.to_str().unwrap()], &Ctx::new(dir.path(), &[]));
        (code, out)
    }

    #[test]
    fn home_path_is_flagged() {
        let (code, out) = run_file("a.md", &format!("see /home/{}/rsi/docs\n", account()));
        assert_eq!(code, 1);
        assert!(
            out.contains("a.md:1: personal path or account name"),
            "{out}"
        );
    }

    #[test]
    fn ssh_account_is_flagged() {
        assert_eq!(run_file("a.sh", &format!("ssh {}@host\n", account())).0, 1);
    }

    #[test]
    fn public_ip_is_flagged() {
        let (code, out) = run_file("a.sh", &format!("host={}\n", public_ip_text()));
        assert_eq!(code, 1);
        assert!(
            out.contains(&format!("public IP literal {}", public_ip_text())),
            "{out}"
        );
    }

    #[test]
    fn placeholders_and_private_ranges_pass() {
        let text = [
            "host=remote.example.net",
            "doc=203.0.113.10 lan=192.168.1.5 lo=127.0.0.1 any=0.0.0.0",
            "cgnat=100.101.102.103 meta=169.254.169.254 vpc=10.61.0.0",
            "home=$HOME/rsi ~/rsi /home/user/rsi",
        ]
        .join("\n");
        assert_eq!(run_file("a.txt", &text), (0, String::new()));
    }

    #[test]
    fn multicast_and_reserved_literals_are_not_public_hosts() {
        let text = [
            "mc=224.0.0.1 ssdp=239.255.255.250 edge=224.0.0.0 top=239.255.255.255",
            "reserved=240.0.0.1 bcast=255.255.255.255",
        ]
        .join("\n");
        assert_eq!(run_file("a.rs", &text), (0, String::new()));
        // The boundary neighbours are still ordinary globally routable hosts.
        let below = ["223", "255", "255", "255"].join(".");
        assert_eq!(run_file("a.rs", &format!("\"{below}\"\n")).0, 1);
    }

    #[test]
    fn multicast_does_not_weaken_personal_path_detection() {
        let (code, out) = run_file("a.rs", &format!("\"224.0.0.1\" /home/{}/x\n", account()));
        assert_eq!(code, 1);
        assert!(
            out.contains("a.rs:1: personal path or account name"),
            "{out}"
        );
        assert!(!out.contains("public IP literal"));
    }

    #[test]
    fn allowlist_is_only_the_github_slug_and_public_resolver() {
        assert_eq!(
            run_file(
                "Cargo.toml",
                &format!("repository = \"https://github.com/{}/rsi\"\n", account())
            )
            .0,
            0
        );
        assert_eq!(run_file("a.rs", "let dns = \"8.8.8.8\";\n").0, 0);
        assert_eq!(
            run_file(
                "a.rs",
                &format!("url = \"github.com/{0}/rsi\" /home/{0}\n", account())
            )
            .0,
            1
        );
        assert_eq!(
            run_file("a.rs", &format!("let dns = \"{}\";\n", other_resolver())).0,
            1
        );
    }

    #[test]
    fn egress_fixture_files_may_name_public_ips_but_not_personal_paths() {
        let rel = "crates/rsi-common/src/egress_policy.rs";
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path(), &[]);
        write(
            dir.path(),
            rel,
            &format!("assert_public(\"{}\");\n", public_ip_text()),
        );
        assert_eq!(crate::testutil::run(main, &[rel], &ctx).0, 0);
        write(
            dir.path(),
            rel,
            &format!(
                "let p = \"/home/{}/x\"; assert_public(\"{}\");\n",
                account(),
                public_ip_text()
            ),
        );
        assert_eq!(crate::testutil::run(main, &[rel], &ctx).0, 1);
        // The same line in any other file is still a finding.
        assert_eq!(
            run_file(
                "egress_policy.rs",
                &format!("assert_public(\"{}\");\n", public_ip_text())
            )
            .0,
            1
        );
    }

    #[test]
    fn version_numbers_are_not_ip_literals() {
        assert_eq!(
            run_file("a.txt", "v1.94.1 and 1.2.3.4.5 and 12.3.4.5.6\n").0,
            0
        );
    }

    #[test]
    fn thoughts_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path(), &[]);
        assert_eq!(
            crate::testutil::run(main, &["thoughts/shared/x.md"], &ctx).0,
            0
        );
    }

    fn staged_run(dir: &Path) -> (i32, String) {
        let (code, out, _) = crate::testutil::run(main, &["--staged"], &Ctx::new(dir, &[]));
        (code, out)
    }

    fn make_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        dir
    }

    #[test]
    fn merge_inheriting_other_parents_finding_passes() {
        let d = make_repo();
        let d = d.path();
        write(d, "t.txt", "base\n");
        git(d, &["add", "t.txt"]);
        git(d, &["commit", "-q", "-m", "base"]);
        git(d, &["branch", "other"]);
        write(d, "ours.txt", "ours\n");
        git(d, &["add", "ours.txt"]);
        git(d, &["commit", "-q", "-m", "ours"]);
        git(d, &["checkout", "-q", "other"]);
        write(d, "t.txt", &format!("base\nhost={}\n", public_ip_text()));
        git(d, &["add", "t.txt"]);
        git(d, &["commit", "-q", "-m", "other"]);
        git(d, &["checkout", "-q", "-"]);
        git(d, &["merge", "--no-ff", "--no-commit", "other"]);
        assert!(d.join(".git/MERGE_HEAD").is_file());
        assert_eq!(staged_run(d), (0, String::new()));
    }

    #[test]
    fn merge_resolution_refuses_only_new_findings() {
        let d = make_repo();
        let d = d.path();
        write(d, "t.txt", "base\n");
        git(d, &["add", "t.txt"]);
        git(d, &["commit", "-q", "-m", "base"]);
        git(d, &["branch", "other"]);
        write(d, "t.txt", "ours\n");
        git(d, &["add", "t.txt"]);
        git(d, &["commit", "-q", "-m", "ours"]);
        git(d, &["checkout", "-q", "other"]);
        write(d, "t.txt", &format!("theirs\nhost={}\n", public_ip_text()));
        git(d, &["add", "t.txt"]);
        git(d, &["commit", "-q", "-m", "other"]);
        git(d, &["checkout", "-q", "-"]);
        assert!(
            !git_status(d, &["merge", "--no-ff", "--no-commit", "other"])
                .status
                .success()
        );
        assert!(d.join(".git/MERGE_HEAD").is_file());
        // The inherited finding moves down a line in the resolved index.
        write(
            d,
            "t.txt",
            &format!(
                "resolved\nclean\nhost={}\nnew={}\n",
                public_ip_text(),
                other_resolver()
            ),
        );
        git(d, &["add", "t.txt"]);
        let (code, out) = staged_run(d);
        assert_eq!(code, 1);
        assert!(
            out.contains(&format!("t.txt:4: public IP literal {}", other_resolver())),
            "{out}"
        );
        assert!(out.contains("check-personal-paths: 1 finding(s)."), "{out}");
    }

    #[test]
    fn preexisting_matches_do_not_block_an_unrelated_edit() {
        let d = make_repo();
        let d = d.path();
        write(
            d,
            "t.mjs",
            &format!(
                "a\nhost={}\nhome=/home/{}/x\nb\n",
                public_ip_text(),
                account()
            ),
        );
        git(d, &["add", "t.mjs"]);
        git(d, &["commit", "-q", "-m", "seed"]);
        write(
            d,
            "t.mjs",
            &format!(
                "a\nhost={}\nhome=/home/{}/x\nb\nc clean\n",
                public_ip_text(),
                account()
            ),
        );
        git(d, &["add", "t.mjs"]);
        assert_eq!(staged_run(d), (0, String::new()));
    }

    #[test]
    fn newly_added_personal_path_and_public_ip_are_refused_with_new_line_numbers() {
        let d = make_repo();
        let d = d.path();
        write(d, "t.mjs", &format!("a\nhost={}\nb\n", public_ip_text()));
        git(d, &["add", "t.mjs"]);
        git(d, &["commit", "-q", "-m", "seed"]);
        let text = format!(
            "a\nhost={}\nb\nx=/home/{}/y\nip={}\n",
            public_ip_text(),
            account(),
            other_resolver()
        );
        write(d, "t.mjs", &text);
        git(d, &["add", "t.mjs"]);
        let (code, out) = staged_run(d);
        assert_eq!(code, 1);
        assert!(
            out.contains("t.mjs:4: personal path or account name"),
            "{out}"
        );
        assert!(
            out.contains(&format!("t.mjs:5: public IP literal {}", other_resolver())),
            "{out}"
        );
        assert!(!out.contains("t.mjs:2:"));
    }

    #[test]
    fn new_file_is_judged_fully_and_thoughts_and_deletions_are_skipped() {
        let d = make_repo();
        let d = d.path();
        write(d, "old.txt", &format!("/home/{}/old\n", account()));
        git(d, &["add", "old.txt"]);
        git(d, &["commit", "-q", "-m", "seed"]);
        git(d, &["rm", "-q", "old.txt"]);
        write(d, "thoughts/n.md", &format!("/home/{}/n\n", account()));
        git(d, &["add", "thoughts/n.md"]);
        assert_eq!(staged_run(d), (0, String::new()));
        write(d, "new.txt", &format!("ok\n/home/{}/new\n", account()));
        git(d, &["add", "new.txt"]);
        let (code, out) = staged_run(d);
        assert_eq!(code, 1);
        assert!(
            out.contains("new.txt:2: personal path or account name"),
            "{out}"
        );
    }

    #[test]
    fn added_line_starting_with_plus_markers_is_still_judged() {
        let diff = format!(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1,2 @@\n x\n+++ /home/{}/z\n",
            account()
        );
        assert_eq!(
            scan_diff(&diff),
            vec![(
                "f".to_string(),
                2,
                "personal path or account name".to_string()
            )]
        );
    }

    #[test]
    fn ip_classification_follows_python_ipaddress() {
        for host in [
            "0.1.2.3",
            "10.0.0.1",
            "172.31.0.1",
            "192.0.0.8",
            "198.19.0.1",
            "100.64.0.1",
            "01.2.3.4",
            "1.2.3.256",
        ] {
            assert!(!public_ip(host), "{host}");
        }
        for host in [
            ["192", "0", "0", "9"],
            ["172", "32", "0", "1"],
            ["100", "128", "0", "1"],
        ] {
            assert!(public_ip(&host.join(".")), "{host:?}");
        }
    }
}
