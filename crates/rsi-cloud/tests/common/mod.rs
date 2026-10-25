//! Shared fixtures: the ledger shapes and a runner for the `rsi-cloud` binary.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub const HEADER: &str = "# AWS satellite spend ledger\n\n\
Operator grant: $100 starting 2026-09-27 22:48:22 UTC (direct operator message).\n\
Configured compute estimate: $3.5616/hour. Stop and report by $90 cumulative under this grant.\n";

pub fn repo_root() -> PathBuf {
    Path::new(rsi_common::manifest_dir!()).join("../..")
}

pub fn rsi_cloud() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rsi-cloud"))
}

pub struct Ran {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    fn from(output: &Output) -> Self {
        Self {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// Run `rsi-cloud ARGS` with `stdin` and return its exit code and output.
pub fn run(args: &[&str], stdin: &str) -> Ran {
    let mut child = rsi_cloud()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rsi-cloud");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    Ran::from(&child.wait_with_output().expect("wait rsi-cloud"))
}
