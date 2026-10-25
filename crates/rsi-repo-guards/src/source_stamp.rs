//! Content identity shared by build.rs and the staged admission bootstrap.
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn sources(directory: &Path, result: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sources(&entry.path(), result)?;
        } else if entry.path().extension().is_some_and(|s| s == "rs") {
            result.push(entry.path());
        }
    }
    Ok(())
}

pub fn input_paths(root: &Path) -> io::Result<Vec<PathBuf>> {
    let crate_dir = root.join("crates/rsi-repo-guards");
    let mut paths = vec![
        root.join("Cargo.toml"),
        root.join("Cargo.lock"),
        root.join("rust-toolchain.toml"),
        crate_dir.join("Cargo.toml"),
        crate_dir.join("build.rs"),
    ];
    sources(&crate_dir.join("src"), &mut paths)?;
    paths.sort();
    Ok(paths)
}

/// Hash names plus lengths plus bytes, so renames and concatenation ambiguity
/// cannot produce the same input stream. Git supplies the repository's native
/// content hash; no interpreter or mutable timestamp is trusted.
pub fn stamp(root: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    for path in input_paths(root)? {
        let relative = path.strip_prefix(root).map_err(io::Error::other)?;
        let contents = fs::read(&path)?;
        bytes.extend_from_slice(relative.to_string_lossy().replace('\\', "/").as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(contents.len().to_string().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&contents);
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing git stdin"))?
        .write_all(&bytes)?;
    let result = child.wait_with_output()?;
    if !result.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&result.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

pub fn verify(root: &Path, expected: &str) -> io::Result<()> {
    if stamp(root)? == expected {
        Ok(())
    } else {
        Err(io::Error::other(
            "prebuilt runner differs from this checkout; rebuild through an admitted daemon build job",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_stamp_detects_source_and_lock_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("crates/rsi-repo-guards/src")).unwrap();
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "crates/rsi-repo-guards/Cargo.toml",
            "crates/rsi-repo-guards/build.rs",
            "crates/rsi-repo-guards/src/lib.rs",
        ] {
            fs::write(root.join(path), "fixture").unwrap();
        }
        let before = stamp(root).unwrap();
        assert_eq!(stamp(root).unwrap(), before);
        fs::write(root.join("crates/rsi-repo-guards/src/lib.rs"), "changed").unwrap();
        let source = stamp(root).unwrap();
        assert_ne!(source, before);
        fs::write(root.join("Cargo.lock"), "changed").unwrap();
        assert_ne!(stamp(root).unwrap(), source);
        assert!(verify(root, &before).is_err());
    }
}
