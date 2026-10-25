//! The cloud spend safety net (#3767): the spend guard, the orphan watchdog and
//! the spot-pool lease state, ported from `scripts/cloud-spend.py`,
//! `scripts/cloud-watchdog.py` and `scripts/cloud-pool-state.py` with the same
//! command lines, exit codes, output and file formats.
//!
//! The binary is `rsi-cloud <aws|spend|watchdog|pool-state|epoch-stamp|sweep-json|sweep-report|sweep-verdict> ...`.
//! The `aws` command signs the gate/pool's small API surface over HTTPS.
//! The separate watchdog still uses [`watchdog::CliAws`]; tests use fixtures.

pub mod args;
pub mod aws;
pub mod pool_state;
pub mod spend;
pub mod sweep;
pub mod util;
pub mod watchdog;

use std::io::{Read, Write};

/// Run `rsi-cloud` with `argv` (without the program name). Returns the exit code.
pub fn run(
    argv: &[String],
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let Some((command, rest)) = argv.split_first() else {
        let _ = writeln!(
            stderr,
            "usage: rsi-cloud <aws|spend|watchdog|pool-state|epoch-stamp|sweep-json|sweep-report|sweep-verdict> ..."
        );
        return 2;
    };
    match command.as_str() {
        "aws" => aws::run(rest, stdout, stderr),
        "spend" => spend::run(rest, stdin, stdout, stderr),
        "watchdog" => watchdog::run(rest, stdout, stderr),
        "pool-state" => pool_state::run(rest, stdin, stdout, stderr),
        "epoch-stamp" => util::run_epoch_stamp(rest, stdout, stderr),
        "sweep-json" | "sweep-report" | "sweep-verdict" => {
            sweep::run(command, rest, stdout, stderr)
        }
        other => {
            let _ = writeln!(stderr, "rsi-cloud: unknown command {other}");
            let _ = writeln!(
                stderr,
                "usage: rsi-cloud <aws|spend|watchdog|pool-state|epoch-stamp|sweep-json|sweep-report|sweep-verdict> ..."
            );
            2
        }
    }
}
