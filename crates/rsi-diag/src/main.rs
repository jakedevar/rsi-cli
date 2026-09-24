//! `rsi-diag` — read-only diagnostic CLI for the Flywheel daemon (RSI-010).
//!
//! Subcommands:
//!
//! - `mismatch-report` — aggregate per-command capability-class mismatch rates
//!   by reading `~/.rsi/rsi.db` in `READ_ONLY` mode. Read-only by design: a
//!   diagnostic CLI has no business performing mutations or schema changes,
//!   which under the CLAUDE.md "Database Rules" require a migration (schema)
//!   or explicit user consent (hard deletes).
//! - `schema` — introspect the live SQLite schema (tables, columns, indexes,
//!   foreign keys, `user_version`) so agents can discover structure without
//!   reverse-engineering migrations. Also read-only.

mod mismatch_report;
mod schema;

use std::path::PathBuf;
use std::process::ExitCode;

fn print_usage() {
    eprintln!(
        "rsi-diag — read-only diagnostic CLI for rsid\n\n\
         USAGE:\n  \
             rsi-diag <SUBCOMMAND>\n\n\
         SUBCOMMANDS:\n  \
             mismatch-report    Aggregate capability-class mismatches (RSI-010)\n  \
             schema             Inspect the live SQLite schema (tables/columns/indexes)\n\n\
         Run `rsi-diag <SUBCOMMAND> --help` for subcommand-specific options."
    );
}

fn default_db_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".rsi").join("rsi.db"))
        .unwrap_or_else(|| PathBuf::from("rsi.db"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::from(2);
    }

    match args[0].as_str() {
        "mismatch-report" => {
            let rest = &args[1..];
            if rest.iter().any(|a| a == "-h" || a == "--help") {
                eprintln!(
                    "rsi-diag mismatch-report — aggregate declared-vs-actual capability class pairs\n\n\
                     USAGE:\n  \
                         rsi-diag mismatch-report [--db <PATH>] [--since <RFC3339>] [--json]\n\n\
                     FLAGS:\n  \
                         --db      Path to sqlite DB (default: ~/.rsi/rsi.db)\n  \
                         --since   Only include sessions with created_at >= RFC3339 timestamp\n  \
                         --json    Emit JSON instead of the human table\n"
                );
                return ExitCode::SUCCESS;
            }
            let mut db_path: Option<PathBuf> = None;
            let mut since: Option<String> = None;
            let mut json = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--db" => {
                        i += 1;
                        match rest.get(i) {
                            Some(p) => db_path = Some(PathBuf::from(p)),
                            None => {
                                eprintln!("--db requires a path argument");
                                return ExitCode::from(2);
                            }
                        }
                    }
                    "--since" => {
                        i += 1;
                        match rest.get(i) {
                            Some(s) => since = Some(s.clone()),
                            None => {
                                eprintln!("--since requires an RFC3339 timestamp");
                                return ExitCode::from(2);
                            }
                        }
                    }
                    "--json" => json = true,
                    other => {
                        eprintln!("unknown argument: {}", other);
                        return ExitCode::from(2);
                    }
                }
                i += 1;
            }
            let path = db_path.unwrap_or_else(default_db_path);
            match mismatch_report::run(&path, since.as_deref(), json) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("mismatch-report failed: {}", e);
                    ExitCode::from(1)
                }
            }
        }
        "schema" => {
            let rest = &args[1..];
            if rest.iter().any(|a| a == "-h" || a == "--help") {
                eprintln!(
                    "rsi-diag schema — inspect the live SQLite schema (read-only)\n\n\
                     USAGE:\n  \
                         rsi-diag schema [--db <PATH>] [--json]                    List tables with row counts\n  \
                         rsi-diag schema <TABLE> [--db <PATH>] [--json]            Columns, indexes, FKs for one table\n  \
                         rsi-diag schema --grep <PATTERN> [--db <PATH>] [--json]   Tables/columns matching a substring\n  \
                         rsi-diag schema --version [--db <PATH>] [--json]          Print PRAGMA user_version (applied schema)\n\n\
                     FLAGS:\n  \
                         --db        Path to sqlite DB (default: ~/.rsi/rsi.db)\n  \
                         --grep      Filter to tables/columns whose name contains PATTERN (case-insensitive)\n  \
                         --version   Print PRAGMA user_version and exit\n  \
                         --json      Emit JSON instead of the human table\n\n\
                     The schema's source of truth is the live DB; this command never mutates it."
                );
                return ExitCode::SUCCESS;
            }
            let mut db_path: Option<PathBuf> = None;
            let mut grep: Option<String> = None;
            let mut version = false;
            let mut json = false;
            let mut table: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--db" => {
                        i += 1;
                        match rest.get(i) {
                            Some(p) => db_path = Some(PathBuf::from(p)),
                            None => {
                                eprintln!("--db requires a path argument");
                                return ExitCode::from(2);
                            }
                        }
                    }
                    "--grep" => {
                        i += 1;
                        match rest.get(i) {
                            Some(p) => grep = Some(p.clone()),
                            None => {
                                eprintln!("--grep requires a pattern argument");
                                return ExitCode::from(2);
                            }
                        }
                    }
                    "--version" => version = true,
                    "--json" => json = true,
                    other if other.starts_with('-') => {
                        eprintln!("unknown argument: {}", other);
                        return ExitCode::from(2);
                    }
                    other => {
                        if table.is_some() {
                            eprintln!("unexpected extra argument: {}", other);
                            return ExitCode::from(2);
                        }
                        table = Some(other.to_string());
                    }
                }
                i += 1;
            }
            // Modes are mutually exclusive; reject ambiguous combinations rather
            // than silently picking a precedence an agent can't predict.
            if version && (table.is_some() || grep.is_some()) {
                eprintln!("--version takes no table argument or --grep");
                return ExitCode::from(2);
            }
            if table.is_some() && grep.is_some() {
                eprintln!("--grep cannot be combined with a table argument");
                return ExitCode::from(2);
            }
            let path = db_path.unwrap_or_else(default_db_path);
            match schema::run(&path, table.as_deref(), grep.as_deref(), version, json) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("schema failed: {}", e);
                    ExitCode::from(1)
                }
            }
        }
        "-h" | "--help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown subcommand: {}", other);
            print_usage();
            ExitCode::from(2)
        }
    }
}
