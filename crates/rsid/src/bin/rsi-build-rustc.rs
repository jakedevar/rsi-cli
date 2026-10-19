//! Cooperative machine-wide compiler slots for Cargo launched by agents.
//!
//! Cargo invokes RUSTC_WRAPPER with the real rustc path as argv[1]. The
//! wrapper holds one process-independent flock while the compiler (or
//! sccache client) runs. Idle model sessions and Cargo processes hold none.
//!
//! Shared targets (#1741): a sandbox seeded by `scripts/target-pool` holds
//! hard links to a pool's outputs. The linker writes executables and split
//! debuginfo into an existing file, so before the compiler runs the wrapper
//! unlinks this unit's outputs that are shared (more than one link) or
//! read-only (the pool marks them so). The compiler then writes a new file and
//! no other sandbox's copy changes. `rsi-build-rustc --capabilities` lists
//! `unlink-outputs` so the seeder links those outputs only behind this wrapper.
//! With sccache, `CARGO_TARGET_DIR` is removed from the compiler environment:
//! rustc never reads it, but sccache hashes every `CARGO_*` variable, so a
//! per-sandbox target path made every cache key unique to one sandbox.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

fn slot_root() -> PathBuf {
    std::env::var_os("RSI_BUILD_SLOT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .expect("home directory required")
                .join(".rsi/build-slots")
        })
}

fn try_slot(root: &PathBuf, count: u32) -> std::io::Result<File> {
    for slot in 0..count {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join(format!("slot-{slot}")))?;
        // flock is an advisory, process-independent lock. The file stays
        // open until the compiler child settles and this wrapper exits.
        let result =
            unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
        if result == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error);
        }
    }
    Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
}

/// What this wrapper guarantees to `scripts/target-pool`.
const CAPABILITIES: &str = "unlink-outputs sccache-target-dir-neutral";

/// `--out-dir` and the `<crate><extra-filename>` stem of one rustc invocation.
fn unit_outputs(args: &[OsString]) -> Option<(PathBuf, String)> {
    let mut out_dir = None;
    let mut crate_name = None;
    let mut extra = None;
    let mut iter = args.iter().map(|arg| arg.to_string_lossy());
    while let Some(arg) = iter.next() {
        let codegen = |value: &str| value.strip_prefix("extra-filename=").map(str::to_string);
        match arg.as_ref() {
            "--out-dir" => out_dir = iter.next().map(|value| PathBuf::from(value.as_ref())),
            "--crate-name" => crate_name = iter.next().map(|value| value.to_string()),
            "-C" | "--codegen" => {
                if let Some(value) = iter.next().as_deref().and_then(codegen) {
                    extra = Some(value);
                }
            }
            other => {
                if let Some(value) = other.strip_prefix("--out-dir=") {
                    out_dir = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--crate-name=") {
                    crate_name = Some(value.to_string());
                } else if let Some(value) = other
                    .strip_prefix("-C")
                    .or_else(|| other.strip_prefix("--codegen="))
                    .and_then(codegen)
                {
                    extra = Some(value);
                }
            }
        }
    }
    // Without an extra filename the stem is not unique to this unit
    // (`rustc -vV`, a plain build script probe); leave such outputs alone.
    let extra = extra.filter(|value| !value.is_empty())?;
    Some((out_dir?, format!("{}{extra}", crate_name?)))
}

/// Whether `name` is an output of the unit with this stem: the executable
/// `<stem>`, `lib<stem>.{rlib,rmeta,so,...}`, `<stem>.<anything>`, or a name
/// rustc shortened because it was too long, which keeps the unit's unique
/// `-<hash>` extra filename followed by `.` (`182a…-_tail-<hash>.x.dwo`).
fn is_unit_output(name: &OsStr, stem: &str) -> bool {
    let name = name.to_string_lossy();
    let rest = name.strip_prefix("lib").unwrap_or(&name);
    let prefixed = [name.as_ref(), rest].iter().any(|candidate| {
        candidate
            .strip_prefix(stem)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with('.'))
    });
    let hash = stem
        .rfind('-')
        .map(|index| &stem[index..])
        .filter(|hash| hash.len() > 8);
    prefixed || hash.is_some_and(|hash| name.contains(&format!("{hash}.")))
}

fn shared_or_read_only(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return true;
        }
    }
    meta.permissions().readonly()
}

/// Unlink this unit's outputs that another target shares or that are
/// read-only, so the compiler and linker create new files (#1741).
fn unlink_shared_outputs(args: &[OsString]) -> std::io::Result<usize> {
    let Some((out_dir, stem)) = unit_outputs(args) else {
        return Ok(0);
    };
    let entries = match std::fs::read_dir(&out_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        if !is_unit_output(&entry.file_name(), &stem) {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.is_file() && shared_or_read_only(&meta) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(removed)
}

fn run() -> std::io::Result<ExitCode> {
    let mut args = std::env::args_os().skip(1);
    let rustc = args.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing rustc path")
    })?;
    if rustc == "--capabilities" {
        println!("{CAPABILITIES}");
        return Ok(ExitCode::SUCCESS);
    }
    let args: Vec<OsString> = args.collect();
    let count = std::env::var("RSI_BUILD_SLOTS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|count| (1..=64).contains(count))
        .unwrap_or(16);
    let root = slot_root();
    std::fs::create_dir_all(&root)?;
    let _lease = loop {
        match try_slot(&root, count) {
            Ok(lease) => break lease,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(error) => return Err(error),
        }
    };
    unlink_shared_outputs(&args)?;
    let cache = std::env::var_os("RSI_BUILD_SCCACHE");
    let program = cache.as_ref().unwrap_or(&rustc);
    let mut compiler = Command::new(program);
    if cache.is_some() {
        compiler.arg(&rustc);
        compiler.env_remove("CARGO_TARGET_DIR");
    }
    let status = compiler.args(&args).status()?;
    Ok(ExitCode::from(
        status.code().unwrap_or(1).clamp(0, 255) as u8
    ))
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("rsi-build-rustc: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_blocks_other_compilers_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        let lease = try_slot(&root, 1).unwrap();
        assert_eq!(
            try_slot(&root, 1).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(lease);
        assert!(try_slot(&root, 1).is_ok());
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn unit_outputs_reads_cargo_style_arguments() {
        let spaced = args(&[
            "--crate-name",
            "rsid",
            "--edition=2024",
            "src/lib.rs",
            "-C",
            "metadata=abc",
            "-C",
            "extra-filename=-0123456789abcdef",
            "--out-dir",
            "/t/debug/deps",
        ]);
        assert_eq!(
            unit_outputs(&spaced),
            Some((
                PathBuf::from("/t/debug/deps"),
                "rsid-0123456789abcdef".to_string()
            ))
        );
        let joined = args(&[
            "--crate-name=app",
            "-Cextra-filename=-ffff",
            "--out-dir=/x/deps",
        ]);
        assert_eq!(
            unit_outputs(&joined),
            Some((PathBuf::from("/x/deps"), "app-ffff".to_string()))
        );
        assert_eq!(unit_outputs(&args(&["-vV"])), None);
        assert_eq!(
            unit_outputs(&args(&["--crate-name", "x", "--out-dir", "/d"])),
            None,
            "no extra filename: the stem is not unique, leave outputs alone"
        );
    }

    #[test]
    fn unit_output_names_match_only_this_unit() {
        let stem = "rsid-0123456789abcdef";
        for name in [
            "rsid-0123456789abcdef",
            "librsid-0123456789abcdef.rlib",
            "librsid-0123456789abcdef.rmeta",
            "rsid-0123456789abcdef.d",
            "rsid-0123456789abcdef.rsid.1a2b-cgu.0.rcgu.dwo",
            // rustc shortens a too-long output name to `<hash>-<tail>`.
            "182a3f38718e32da-_long_tail-0123456789abcdef.long_crate.b8f5-cgu.0.rcgu.dwo",
        ] {
            assert!(is_unit_output(OsStr::new(name), stem), "{name}");
        }
        for name in [
            "rsid-0123456789abcdeff",
            "librsid_store-fedcba9876543210.rlib",
            "rsid-1111111111111111",
        ] {
            assert!(!is_unit_output(OsStr::new(name), stem), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn shared_and_read_only_outputs_are_unlinked_and_the_other_link_keeps_its_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("deps");
        let pool = dir.path().join("pool");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::create_dir_all(&pool).unwrap();
        // A pool executable hard-linked into this target.
        std::fs::write(pool.join("app-aaaa"), b"pool bytes").unwrap();
        std::fs::hard_link(pool.join("app-aaaa"), deps.join("app-aaaa")).unwrap();
        // A read-only split-debuginfo file with one link (the pool was removed).
        let dwo = deps.join("app-aaaa.app.1-cgu.0.rcgu.dwo");
        std::fs::write(&dwo, b"dwo").unwrap();
        std::fs::set_permissions(&dwo, std::fs::Permissions::from_mode(0o444)).unwrap();
        // This sandbox's own writable rlib and another unit's shared file stay.
        std::fs::write(deps.join("libapp-aaaa.rlib"), b"own").unwrap();
        std::fs::write(pool.join("other-bbbb"), b"other").unwrap();
        std::fs::hard_link(pool.join("other-bbbb"), deps.join("other-bbbb")).unwrap();

        let removed = unlink_shared_outputs(&args(&[
            "--crate-name",
            "app",
            "-C",
            "extra-filename=-aaaa",
            "--out-dir",
            deps.to_str().unwrap(),
        ]))
        .unwrap();

        assert_eq!(removed, 2);
        assert!(!deps.join("app-aaaa").exists());
        assert!(!dwo.exists());
        assert_eq!(
            std::fs::read(deps.join("libapp-aaaa.rlib")).unwrap(),
            b"own"
        );
        assert!(deps.join("other-bbbb").exists());
        assert_eq!(std::fs::read(pool.join("app-aaaa")).unwrap(), b"pool bytes");
    }
}
