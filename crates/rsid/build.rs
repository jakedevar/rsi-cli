use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn collect_store_sources(directory: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_store_sources(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && path.file_name().is_none_or(|name| name != "tests.rs")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn store_source_digest(manifest_directory: &Path) -> io::Result<String> {
    let workspace = manifest_directory
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("rsid manifest is not inside the workspace"))?;
    let mut files = Vec::new();
    collect_store_sources(&manifest_directory.join("src/store"), &mut files)?;
    files.extend([
        manifest_directory.join("Cargo.toml"),
        workspace.join("Cargo.lock"),
        workspace.join("rust-toolchain.toml"),
    ]);
    files.sort();

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rsid-current-schema-template-source-v1\0");
    for path in files {
        let relative = path.strip_prefix(workspace).map_err(io::Error::other)?;
        let relative = relative.to_string_lossy();
        let contents = fs::read(&path)?;
        hasher.update(&(relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update(&(contents.len() as u64).to_le_bytes());
        hasher.update(&contents);
        println!("cargo:rerun-if-changed={}", path.display());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn main() {
    // Get the SQLite include directory from the bundled libsqlite3-sys crate.
    // This provides sqlite3.h / sqlite3ext.h needed by the sqlite-vec extension.
    let sqlite_include = std::env::var("DEP_SQLITE3_INCLUDE").unwrap_or_default();

    let mut build = cc::Build::new();
    build
        .file("vendor/sqlite-vec/sqlite-vec.c")
        .include("vendor/sqlite-vec");

    if !sqlite_include.is_empty() {
        build.include(&sqlite_include);
        // Compile as core extension when we have the SQLite headers
        build.define("SQLITE_CORE", None);
    }

    build.warnings(false).compile("sqlite_vec");

    println!("cargo:rerun-if-changed=vendor/sqlite-vec/sqlite-vec.c");
    println!("cargo:rerun-if-changed=vendor/sqlite-vec/sqlite-vec.h");

    let manifest_directory = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"),
    );
    let digest = store_source_digest(&manifest_directory)
        .expect("hash production Store sources for the test schema-template cache");
    println!("cargo:rustc-env=RSID_TEST_SCHEMA_SOURCE_DIGEST={digest}");
}
