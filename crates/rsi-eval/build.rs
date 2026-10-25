//! Runner build stamp (R3a, design "Outcomes, records, provenance and budgets"): embeds the
//! sha256 of every source the Rust TB2 evaluator's behaviour depends on, as
//! `RSI_EVAL_RUNNER_SOURCE_MANIFEST` (`path=hex;path=hex`, sorted, repository-relative). The
//! daemon verifies it at admission and derives `adapter_tree_hash` from it, so a changed
//! evaluator or reward policy changes the record's observation fingerprint.
//!
//! The text format and the digests built from it are `rsi_common::bench_runner`'s; this script
//! only hashes files. Test files are excluded (a test edit does not change the evaluator), and a
//! file absent at build time is stamped `missing`, which admission refuses.

use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Single files, relative to the workspace root: the CLI dispatch and manifest, the shared
/// boundary and record types, the daemon's bench job, record and transport boundary, the lock.
const FILES: &[&str] = &[
    "Cargo.lock",
    "crates/rsi-eval/Cargo.toml",
    "crates/rsi-eval/src/main.rs",
    "crates/rsi-eval/src/lib.rs",
    "crates/rsi-common/src/bench.rs",
    "crates/rsi-common/src/bench_record.rs",
    "crates/rsi-common/src/bench_runner.rs",
    "crates/rsid/src/agent_jobs/bench.rs",
    "crates/rsid/src/agent_jobs/bench/config.rs",
    "crates/rsid/src/agent_jobs/bench/record.rs",
    "crates/rsid/src/agent_jobs/bench/transport.rs",
];
/// Directories taken whole (recursively), minus test files.
const DIRS: &[&str] = &["crates/rsi-eval/src/tb2_runner", "crates/rsi-eval/src/tb2"];

fn collect(root: &Path, dir: &str, out: &mut Vec<String>) {
    let Ok(read) = fs::read_dir(root.join(dir)) else {
        return;
    };
    let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{dir}/{name}");
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect(root, &rel, out);
        } else if kind.is_file() && name.ends_with(".rs") && !name.ends_with("tests.rs") {
            out.push(rel);
        }
    }
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let root = manifest_dir.join("../..");
    let mut paths: Vec<String> = FILES.iter().map(|p| (*p).to_string()).collect();
    for dir in DIRS {
        println!("cargo:rerun-if-changed={}", root.join(dir).display());
        collect(&root, dir, &mut paths);
    }
    paths.sort();
    paths.dedup();
    let mut parts = Vec::new();
    for path in &paths {
        let full = root.join(path);
        println!("cargo:rerun-if-changed={}", full.display());
        let value = fs::read(&full).map_or_else(
            |_| "missing".to_string(),
            |bytes| format!("{:x}", Sha256::digest(&bytes)),
        );
        parts.push(format!("{path}={value}"));
    }
    println!(
        "cargo:rustc-env=RSI_EVAL_RUNNER_SOURCE_MANIFEST={}",
        parts.join(";")
    );
}
