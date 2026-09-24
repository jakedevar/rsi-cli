//! `rsi-manifest-validate` — CLI for verification manifest validation.
//!
//! Usage:
//!   rsi-manifest-validate <path>
//!
//! Exit codes:
//!   0 — manifest is valid
//!   1 — I/O or argument error
//!   2 — manifest is invalid

use std::io::Write as _;
use std::process::ExitCode;

use rsi_common::verification_manifest::validate;

fn print_usage() {
    eprintln!(
        "usage: rsi-manifest-validate <path>\n\
         \n\
         Validates a verification manifest markdown file.\n\
         Exit codes: 0=valid, 1=I/O or arg error, 2=invalid manifest."
    );
}

fn main() -> ExitCode {
    let mut path: Option<String> = None;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return ExitCode::from(0);
            }
            other if other.starts_with("--") => {
                eprintln!("error: unknown flag `{other}`");
                print_usage();
                return ExitCode::from(1);
            }
            other => {
                if path.is_some() {
                    eprintln!("error: only one path argument is allowed");
                    return ExitCode::from(1);
                }
                path = Some(other.to_string());
            }
        }
    }

    let Some(path) = path else {
        eprintln!("error: missing <path> argument");
        print_usage();
        return ExitCode::from(1);
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!("I/O error: {err} (path: {path})");
            return ExitCode::from(1);
        }
    };

    let result = validate(&content);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if let Err(err) = serde_json::to_writer_pretty(&mut out, &result) {
        eprintln!("internal error: failed to serialize validation result: {err}");
        return ExitCode::from(1);
    }
    let _ = out.write_all(b"\n");

    if !result.valid {
        let mut stderr = std::io::stderr().lock();
        for err in &result.errors {
            let _ = writeln!(
                stderr,
                "Field `{}` failed rule `{}`: {}",
                err.field, err.rule, err.message
            );
        }
        return ExitCode::from(2);
    }

    ExitCode::from(0)
}
