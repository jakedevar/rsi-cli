//! Handoff validator CLI.
//!
//! Reads a markdown handoff document, runs `rsi_common::handoff_schema::validate`
//! against it, and emits structured output. This binary is shelled out from
//! `.claude/commands/create_handoff.md` (strict mode, write-time gate) and
//! `.claude/commands/resume_handoff.md` (lenient mode, resume-time tolerance).
//!
//! Exit codes:
//! - 0: validation passed
//! - 1: I/O or argument error
//! - 2: validation failed (the document is invalid; stdout/stderr describe why)
//!
//! Stdout: JSON-serialized `Validation` struct (always; even on failure).
//! Stderr: human-readable error lines, one per `ValidationError`.

use std::io::Write as _;
use std::process::ExitCode;

use rsi_common::handoff_schema::{HANDOFF_SCHEMA_VERSION, ValidationMode, validate};

fn print_usage() {
    eprintln!(
        "usage: rsi-handoff-validate [--strict] [--schema-version <N>] <path>\n\
         \n\
         Modes:\n\
           default  lenient (resume-time)\n\
           --strict strict  (write-time)\n\
         \n\
         The schema version must equal {HANDOFF_SCHEMA_VERSION}.\n\
         Exit codes: 0=valid, 1=I/O or arg error, 2=invalid document."
    );
}

fn main() -> ExitCode {
    let mut mode = ValidationMode::Lenient;
    let mut path: Option<String> = None;
    let mut schema_version: Option<u32> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => mode = ValidationMode::Strict,
            "--schema-version" => match args.next() {
                Some(v) => match v.parse::<u32>() {
                    Ok(n) => schema_version = Some(n),
                    Err(_) => {
                        eprintln!("error: --schema-version expects an integer (got `{v}`)");
                        return ExitCode::from(1);
                    }
                },
                None => {
                    eprintln!("error: --schema-version expects an integer argument");
                    return ExitCode::from(1);
                }
            },
            "-h" | "--help" => {
                print_usage();
                return ExitCode::from(0);
            }
            _ if arg.starts_with("--") => {
                eprintln!("error: unknown flag `{arg}`");
                print_usage();
                return ExitCode::from(1);
            }
            _ => {
                if path.is_some() {
                    eprintln!("error: only one path argument is allowed");
                    return ExitCode::from(1);
                }
                path = Some(arg);
            }
        }
    }

    if let Some(v) = schema_version
        && v != HANDOFF_SCHEMA_VERSION
    {
        eprintln!(
            "error: --schema-version {v} unsupported; this binary only knows v{HANDOFF_SCHEMA_VERSION}"
        );
        return ExitCode::from(1);
    }

    let path = match path {
        Some(p) => p,
        None => {
            eprintln!("error: missing <path> argument");
            print_usage();
            return ExitCode::from(1);
        }
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("I/O error: {e} (path: {path})");
            return ExitCode::from(1);
        }
    };

    let result = validate(&content, mode);

    // Always emit JSON to stdout (human-readable lines go to stderr).
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if let Err(e) = serde_json::to_writer(&mut out, &result) {
        // Should never happen — Validation is plain serde-derive types.
        eprintln!("internal error: failed to serialize Validation as JSON: {e}");
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
