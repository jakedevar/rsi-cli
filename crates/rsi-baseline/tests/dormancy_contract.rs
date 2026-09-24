#![allow(clippy::unwrap_used)]
use std::{
    fs,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn fresh_directory() -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "rsi-baseline-dormancy-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).unwrap();
    directory
}

#[test]
fn binary_is_dormant_and_nonpublishing() {
    for args in [Vec::<&str>::new(), vec!["calibrate-baseline"]] {
        let directory = fresh_directory();
        let output = Command::new(env!("CARGO_BIN_EXE_rsi-test-suite-baseline"))
            .current_dir(&directory)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "producer not assembled\n"
        );
        assert!(fs::read_dir(&directory).unwrap().next().is_none());
        fs::remove_dir_all(directory).unwrap();
    }
}
