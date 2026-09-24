//! `rsi-research-validate` — CLI for the v1 research-doc JSON validator.
//!
//! Usage:
//!   rsi-research-validate <path> [--strict] [--schema-version N]
//!
//! Modes:
//!   default  lenient (resume-time tolerance for legacy docs pre-RSI-021)
//!   --strict strict  (write-time gate; same rules as RSI-014's original
//!            single-mode validator)
//!
//! Exit codes:
//!   0 — JSON is valid against the v1 schema in the selected mode.
//!   1 — I/O error (missing file, unreadable).
//!   2 — JSON is invalid (parse error, schema violation, or version mismatch).
//!
//! Stdout:
//!   Pretty-printed `Validation` JSON (`{ valid, errors[], schema_version, mode }`).
//!
//! Stderr:
//!   One line per error: `Field <name> failed rule <rule>: <message>`.
//!
//! Writer-skill semantics: on exit 2, the skill deletes the JSON sidecar and
//! appends `json_companion_status: invalid` to the markdown frontmatter.
//! Consumer-skill semantics: on exit 2, the skill falls back to markdown
//! Read+Grep.

use std::process::ExitCode;

use rsi_common::research_schema::{
    RESEARCH_SCHEMA_VERSION, ValidationMode, validate_research_json_with_mode,
};

fn print_usage() {
    eprintln!(
        "Usage: rsi-research-validate <path> [--strict] [--schema-version N]\n\
         Validates a research-doc JSON sidecar against the v1 schema.\n\
         Modes:\n\
           default  lenient (resume-time)\n\
           --strict strict  (write-time)\n\
         Exit: 0 valid, 1 I/O error, 2 invalid."
    );
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut requested_version: u32 = RESEARCH_SCHEMA_VERSION;
    let mut mode = ValidationMode::Lenient;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => mode = ValidationMode::Strict,
            "--schema-version" => match args.next() {
                Some(v) => match v.parse::<u32>() {
                    Ok(n) => requested_version = n,
                    Err(e) => {
                        eprintln!("invalid --schema-version value `{}`: {}", v, e);
                        return ExitCode::from(2);
                    }
                },
                None => {
                    eprintln!("--schema-version requires a value");
                    return ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                print_usage();
                return ExitCode::from(0);
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag: {}", other);
                return ExitCode::from(2);
            }
            other => {
                if path.is_some() {
                    eprintln!("only one positional <path> argument is allowed");
                    return ExitCode::from(2);
                }
                path = Some(other.to_string());
            }
        }
    }

    let Some(path) = path else {
        print_usage();
        return ExitCode::from(2);
    };

    if requested_version != RESEARCH_SCHEMA_VERSION {
        eprintln!(
            "this binary speaks schema v{}; got --schema-version {}",
            RESEARCH_SCHEMA_VERSION, requested_version
        );
        return ExitCode::from(2);
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("I/O error: {}: {}", path, e);
            return ExitCode::from(1);
        }
    };

    let validation = validate_research_json_with_mode(&content, mode);

    // Stdout: structured JSON for programmatic consumers.
    match serde_json::to_string_pretty(&validation) {
        Ok(s) => println!("{}", s),
        Err(e) => {
            eprintln!("internal: failed to serialize Validation: {}", e);
            return ExitCode::from(1);
        }
    }

    // Stderr: human-readable error lines.
    for err in &validation.errors {
        eprintln!(
            "Field {} failed rule {}: {}",
            err.field, err.rule, err.message
        );
    }

    if validation.valid {
        ExitCode::from(0)
    } else {
        ExitCode::from(2)
    }
}
