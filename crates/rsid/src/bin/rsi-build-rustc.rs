//! Cooperative machine-wide compiler slots for Cargo launched by agents.
//!
//! Cargo invokes RUSTC_WRAPPER with the real rustc path as argv[1]. The
//! wrapper holds one process-independent flock while the compiler (or
//! sccache client) runs. Idle model sessions and Cargo processes hold none.

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

fn run() -> std::io::Result<ExitCode> {
    let mut args = std::env::args_os().skip(1);
    let rustc = args.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing rustc path")
    })?;
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
    let cache = std::env::var_os("RSI_BUILD_SCCACHE");
    let program = cache.as_ref().unwrap_or(&rustc);
    let mut compiler = Command::new(program);
    if cache.is_some() {
        compiler.arg(&rustc);
    }
    let status = compiler.args(args).status()?;
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
}
