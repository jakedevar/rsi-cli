//! #1741: workspace code resolves its package directory at run time.
//!
//! A compile-time manifest path bakes one checkout into the artifact, and Cargo
//! does not rebuild when it changes, so the warm target pool
//! (`scripts/target-pool`) must rebuild every unit that reads it, and every
//! crate that depends on it, in each worker sandbox. Use
//! `rsi_common::manifest_dir!()` (or `manifest_dir!("/suffix")`), or a
//! file-relative `include_str!("../x")` for compile-time reads. Build scripts
//! read the variable from their runtime environment and are exempt.

use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn workspace_code_never_bakes_the_manifest_path_at_compile_time() {
    let crates = Path::new(rsi_common::manifest_dir!()).join("..");
    let mut files = Vec::new();
    rust_sources(&crates, &mut files);
    assert!(
        files.len() > 100,
        "scanned {} files under {}",
        files.len(),
        crates.display()
    );
    let needles: Vec<String> = ["CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH"]
        .iter()
        .flat_map(|name| {
            [
                format!("env!(\"{name}\")"),
                format!("option_env!(\"{name}\")"),
            ]
        })
        .collect();
    let mut offenders = Vec::new();
    for file in &files {
        if file.file_name().is_some_and(|name| name == "build.rs")
            || file.ends_with("rsi-common/src/manifest_dir.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap_or_default();
        for (number, line) in text.lines().enumerate() {
            if needles.iter().any(|needle| line.contains(needle.as_str())) {
                offenders.push(format!(
                    "{}:{}: {}",
                    file.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert_eq!(
        offenders,
        Vec::<String>::new(),
        "use rsi_common::manifest_dir!() or a file-relative include instead"
    );
}
