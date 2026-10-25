//! Legacy execution is confined to the one explicit daemon-owned capture job.
//! Ordinary tests consume the captured golden and execute Rust only.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Deserialize)]
struct Case {
    name: String,
    guard: String,
    args: Vec<String>,
    files: BTreeMap<String, String>,
    ids: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct Golden {
    name: String,
    exit: i32,
    stdout: String,
    stderr: String,
}

fn root() -> PathBuf {
    // Test runtime path only; do not bake a sandbox path into a binary.
    PathBuf::from(rsi_common::manifest_dir!())
}

fn scratch() -> tempfile::TempDir {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(not(unix))]
    let base = std::env::temp_dir();
    let dir = tempfile::Builder::new()
        .prefix("rsi-guard-fixture-")
        .tempdir_in(base)
        .unwrap();
    assert!(
        !dir.path()
            .starts_with(root().parent().unwrap().parent().unwrap())
    );
    dir
}

fn command(program: &str, dir: &Path) -> Command {
    let mut command = Command::new(program);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap());
    command
}

fn git(dir: &Path, args: &[&str]) -> String {
    let result = command("git", dir)
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

fn write(dir: &Path, path: &str, text: &str) {
    let path = dir.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn init(dir: &Path) {
    git(dir, &["init", "-q", "-b", "guard-fixture"]);
    git(dir, &["config", "user.name", "fixture"]);
    git(dir, &["config", "user.email", "fixture@example.invalid"]);
    write(dir, "seed.txt", "seed\n");
    git(dir, &["add", "seed.txt"]);
    git(dir, &["commit", "-qm", "seed"]);
}

fn cases() -> Vec<Case> {
    let mut cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/cases.json")).unwrap();
    // Every endpoint and neighbour of the legacy classification table. Octets
    // keep real public IP literals out of the committed synthetic corpus.
    for (base, bits) in [
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([100, 64, 0, 0], 10),
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
        ([224, 0, 0, 0], 4),
        ([240, 0, 0, 0], 4),
    ] {
        let start = u32::from_be_bytes(base);
        let end = start | (u32::MAX >> bits);
        let mut points = vec![start, end];
        if start > 0 {
            points.push(start - 1);
        }
        if end < u32::MAX {
            points.push(end + 1);
        }
        for value in points {
            let octets = value.to_be_bytes();
            let ip = octets.map(|n| n.to_string()).join(".");
            cases.push(Case {
                name: format!("ipv4-{value}"),
                guard: "personal-paths".into(),
                args: vec!["sample.txt".into()],
                files: BTreeMap::from([("sample.txt".into(), format!("host={ip}\n"))]),
                ids: None,
            });
        }
    }
    for octets in [
        [192, 0, 0, 8],
        [192, 0, 0, 9],
        [192, 0, 0, 10],
        [192, 0, 0, 11],
        [8, 8, 8, 8],
        [8, 8, 4, 4],
    ] {
        let ip = octets.map(|n| n.to_string()).join(".");
        cases.push(Case {
            name: format!("special-{}", u32::from_be_bytes(octets)),
            guard: "personal-paths".into(),
            args: vec!["sample.txt".into()],
            files: BTreeMap::from([("sample.txt".into(), format!("{ip}\n"))]),
            ids: None,
        });
    }
    cases
}

fn run_case(case: &Case) -> Golden {
    let dir = scratch();
    init(dir.path());
    let public = [93, 184, 216, 34].map(|n| n.to_string()).join(".");
    for (path, text) in &case.files {
        write(dir.path(), path, &text.replace("{IP}", &public));
    }
    git(dir.path(), &["add", "--", "."]);
    // Private config is outside the staged source and uses synthetic values.
    if let Some(ids) = &case.ids {
        write(dir.path(), "identifiers", ids);
    }
    let mut cmd = command(env!("CARGO_BIN_EXE_rsi-repo-guard"), dir.path());
    cmd.arg(&case.guard);
    let result = cmd
        .args(&case.args)
        .env("RSI_OPERATOR_IDENTIFIERS_FILE", "identifiers")
        .output()
        .unwrap();
    // Redact generated public address tokens into deterministic octet markers
    // in the golden, retaining complete messages and exact newlines.
    let normalize = |bytes: &[u8]| {
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let text = if cfg!(windows) {
            text.split_inclusive('\n')
                .map(|line| {
                    if let Some((path, rest)) = line.split_once(':') {
                        format!("{}:{rest}", path.replace('\\', "/"))
                    } else {
                        line.to_string()
                    }
                })
                .collect::<String>()
        } else {
            text
        };
        let quad = regex::Regex::new(r"[0-9]{1,3}(?:\.[0-9]{1,3}){3}").unwrap();
        quad.replace_all(&text, |m: &regex::Captures<'_>| {
            format!("<ipv4:{}>", m[0].replace('.', ","))
        })
        .into_owned()
    };
    Golden {
        name: case.name.clone(),
        exit: result.status.code().unwrap(),
        stdout: normalize(&result.stdout),
        stderr: normalize(&result.stderr),
    }
}

#[test]
fn captured_legacy_cli_contracts() {
    let golden: Vec<Golden> = serde_json::from_str(include_str!("fixtures/golden.json")).unwrap();
    let cases = cases();
    assert_eq!(golden.len(), 78);
    assert_eq!(golden.len(), cases.len());
    for (case, expected) in cases.iter().zip(&golden) {
        assert_eq!(&run_case(case), expected, "{}", case.name);
    }
}
