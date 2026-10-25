//! R7 (#3685): prevent runtime, package and gate dependencies on the retired RSI Python path.
#![allow(clippy::unwrap_used)]

use std::path::Path;

const RETIRED: &[&str] = &[
    "scripts/bench-transport",
    "eval/harbor",
    "eval/bench/tb2/pin_check.py",
    "eval/bench/tb2/tb2lib.py",
    "scripts/tests/test_bench_transport.py",
    "scripts/tests/test_eval_harbor.py",
];

const DEPENDENCIES: &[&str] = &[
    "bench-transport",
    "eval/harbor",
    "rsi_harbor",
    "rsi-harbor",
    "test_bench_transport",
    "test_eval_harbor",
    "pin_check.py",
    "tb2lib",
    "TRANSPORT_SCRIPT",
    "ADAPTER_PACKAGE",
    "PIN_FILES",
    "LEAK_AUDIT_SCRIPT",
];

fn dependency(line: &str) -> bool {
    let line = line.trim();
    // Historical source references and documentation are not execution dependencies.
    !line.starts_with('#')
        && !line.starts_with("//")
        && DEPENDENCIES.iter().any(|name| line.contains(name))
}

fn scan(path: &Path, findings: &mut Vec<String>) {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    if metadata.is_symlink() {
        return;
    }
    if metadata.is_dir() {
        if matches!(
            path.file_name().and_then(|s| s.to_str()),
            Some("golden" | "node_modules" | "__pycache__")
        ) {
            return;
        }
        for entry in std::fs::read_dir(path).unwrap() {
            scan(&entry.unwrap().path(), findings);
        }
    } else if path.file_name().and_then(|s| s.to_str()) != Some("tb2_retirement.rs")
        && matches!(
            path.extension().and_then(|s| s.to_str()),
            None | Some("rs" | "py" | "sh" | "toml" | "lock" | "yml" | "yaml" | "json" | "txt")
        )
    {
        let text = std::fs::read_to_string(path).unwrap();
        for (line, text) in text.lines().enumerate() {
            if dependency(text) {
                findings.push(format!("{}:{}: {text}", path.display(), line + 1));
            }
        }
    }
}

#[test]
fn retired_python_has_no_runtime_package_or_gate_consumers() {
    let root = Path::new(rsi_common::manifest_dir!()).join("../..");
    for path in RETIRED {
        assert!(!root.join(path).exists(), "retired input remains: {path}");
    }
    let mut findings = Vec::new();
    for path in [
        "crates",
        "scripts",
        "tools",
        ".github",
        "eval/bench",
        "Makefile",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        scan(&root.join(path), &mut findings);
    }
    assert_eq!(
        findings,
        Vec::<String>::new(),
        "retired Python dependencies"
    );
}

#[test]
fn the_scan_recognizes_import_embedding_packaging_and_test_gate_dependencies() {
    for source in [
        "from rsi_harbor import client",
        "import tb2lib",
        "include_str!(\"../../../scripts/bench-transport\")",
        "pip install eval/harbor",
        "python3 eval/bench/tb2/pin_check.py",
        "tests = [\"scripts/tests/test_eval_harbor.py\"]",
        "TRANSPORT_SCRIPT = retired_path",
    ] {
        assert!(dependency(source), "missed dependency: {source}");
    }
    assert!(!dependency("//! Replaces eval/harbor/rsi_harbor/client.py"));
    assert!(!dependency("# Historical scripts/bench-transport"));
    assert!(!dependency("rsi-eval bench tb2 preflight --plan plan.json"));
}
