//! `rsi-scoped-receipt`: check a `scoped_test` receipt against the contract,
//! compare two runners' receipts on the fields daemon decisions read, or
//! capture the conformance corpus (#3621). Exit 0 when the receipt conforms or
//! the receipts agree, 1 when they do not, 2 on a usage or input error.

use rsi_scoped_receipt::contract::{self, Origin};
use rsi_scoped_receipt::{corpus, parse_line};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage:
  rsi-scoped-receipt check [--runner] [FILE]
      FILE (stdin without one) is a receipt object or a job log ending in a
      SCOPED_TEST_RECEIPT line; --runner judges the runner's own line, which
      never carries the daemon's stamps
  rsi-scoped-receipt compare EXPECTED ACTUAL
      the contract fields on which ACTUAL (say the Rust runner's receipt)
      differs from EXPECTED (the Python runner's, same inputs)
  rsi-scoped-receipt corpus --out FILE [--home DIR] [--forbid WORD]...
      stdin rows `job id<TAB>created_at<TAB>receipt JSON`; writes the newest
      receipt of each shape class, DIR replaced by ~, rows naming WORD dropped";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = run(&args, &mut io::stdin().lock(), &mut io::stdout().lock());
    let code = result.unwrap_or_else(|error| {
        let _ = writeln!(io::stderr(), "rsi-scoped-receipt: {error}");
        2
    });
    ExitCode::from(code)
}

fn run(args: &[String], stdin: &mut dyn Read, out: &mut dyn Write) -> Result<u8, String> {
    let io_error = |error: io::Error| error.to_string();
    match args.first().map(String::as_str) {
        Some("check") => {
            let runner = args.iter().any(|arg| arg == "--runner");
            let files: Vec<&String> = args[1..].iter().filter(|arg| *arg != "--runner").collect();
            if files.len() > 1 {
                return Err(USAGE.into());
            }
            let receipt = read_receipt(files.first().map(|file| Path::new(file.as_str())), stdin)?;
            let origin = if runner {
                Origin::Runner
            } else {
                Origin::Stored
            };
            let violations = contract::check(&receipt, origin);
            for violation in &violations {
                writeln!(out, "{violation}").map_err(io_error)?;
            }
            if violations.is_empty() {
                let present = contract::FIELDS
                    .iter()
                    .filter(|field| receipt.get(field.key).is_some())
                    .count();
                writeln!(
                    out,
                    "conforms: schema {}, {present} of {} contract fields present",
                    rsi_scoped_receipt::SCHEMA,
                    contract::FIELDS.len()
                )
                .map_err(io_error)?;
            }
            Ok(u8::from(!violations.is_empty()))
        }
        Some("compare") if args.len() == 3 => {
            let expected = read_receipt(Some(Path::new(&args[1])), stdin)?;
            let actual = read_receipt(Some(Path::new(&args[2])), stdin)?;
            let differences = contract::compare(&expected, &actual);
            for difference in &differences {
                writeln!(out, "{difference}").map_err(io_error)?;
            }
            if differences.is_empty() {
                writeln!(out, "agree: every contract field matches").map_err(io_error)?;
            }
            Ok(u8::from(!differences.is_empty()))
        }
        Some("corpus") => corpus_command(&args[1..], stdin, out),
        _ => Err(USAGE.into()),
    }
}

/// A receipt object, or the receipt line of a job log, from `file` or stdin.
fn read_receipt(file: Option<&Path>, stdin: &mut dyn Read) -> Result<Value, String> {
    let text = if let Some(path) = file {
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?
    } else {
        let mut text = String::new();
        stdin
            .read_to_string(&mut text)
            .map_err(|error| format!("stdin: {error}"))?;
        text
    };
    let name = file.map_or_else(|| "stdin".to_owned(), |path| path.display().to_string());
    if text.trim_start().starts_with('{') {
        return serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("{name}: not a JSON receipt: {error}"));
    }
    parse_line(&text).ok_or_else(|| {
        format!(
            "{name}: no {}line with a JSON object",
            rsi_scoped_receipt::LINE_PREFIX
        )
    })
}

fn corpus_command(
    args: &[String],
    stdin: &mut dyn Read,
    out: &mut dyn Write,
) -> Result<u8, String> {
    let (mut target, mut home, mut forbidden) = (None, None, Vec::new());
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or(USAGE)?;
        match flag.as_str() {
            "--out" => target = Some(Path::new(value)),
            "--home" => home = Some(value.as_str()),
            "--forbid" => forbidden.push(value.clone()),
            _ => return Err(USAGE.into()),
        }
    }
    let target = target.ok_or(USAGE)?;
    let mut rows = String::new();
    stdin
        .read_to_string(&mut rows)
        .map_err(|error| format!("stdin: {error}"))?;
    let capture = corpus::capture(&rows, home, &forbidden);
    let text: String = capture
        .goldens
        .iter()
        .map(|golden| golden.to_line() + "\n")
        .collect();
    // Written beside the target and renamed, so a stopped job never leaves half a corpus.
    let partial = target.with_extension("jsonl.partial");
    std::fs::write(&partial, text).map_err(|error| format!("{}: {error}", partial.display()))?;
    std::fs::rename(&partial, target).map_err(|error| format!("{}: {error}", target.display()))?;
    writeln!(
        out,
        "corpus: {} rows, {} shape classes, {} goldens written to {} (unparsed {}, over {} bytes {}, naming a forbidden word {})",
        capture.rows,
        capture.classes,
        capture.goldens.len(),
        target.display(),
        capture.unparsed,
        corpus::MAX_RECEIPT_BYTES,
        capture.oversize,
        capture.identity
    )
    .map_err(|error| error.to_string())?;
    Ok(u8::from(capture.goldens.is_empty()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn call(args: &[&str], stdin: &str) -> (Result<u8, String>, String) {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let mut out = Vec::new();
        let code = run(&args, &mut stdin.as_bytes(), &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    const GREEN: &str = r#"{"schema":1,"ok":true,"exit_code":0,"head":"abc","base":"origin/rolling","dry_run":false,"packages":[]}"#;

    #[test]
    fn check_reads_a_job_log_or_a_receipt_and_exits_by_conformance() {
        let log = format!("building\nSCOPED_TEST_RECEIPT {GREEN}\n");
        let (code, out) = call(&["check", "--runner"], &log);
        assert_eq!(code, Ok(0));
        assert_eq!(out, "conforms: schema 1, 7 of 25 contract fields present\n");
        let (code, out) = call(&["check"], r#"{"schema":1,"ok":"yes"}"#);
        assert_eq!(code, Ok(1));
        assert!(
            out.starts_with("ok: expected a bool, found a string\n"),
            "{out}"
        );
        let (code, _) = call(&["check"], "no receipt\n");
        assert!(code.unwrap_err().contains("no SCOPED_TEST_RECEIPT line"));
    }

    #[test]
    fn compare_and_corpus_write_their_results() {
        let dir =
            std::env::temp_dir().join(format!("rsi-scoped-receipt-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (python, rust) = (dir.join("python.json"), dir.join("rust.json"));
        std::fs::write(&python, GREEN).unwrap();
        std::fs::write(&rust, GREEN.replace("\"ok\":true", "\"ok\":false")).unwrap();
        let (code, out) = call(
            &["compare", python.to_str().unwrap(), rust.to_str().unwrap()],
            "",
        );
        assert_eq!(
            (code, out.as_str()),
            (Ok(1), "ok: expected true, found false\n")
        );
        let (code, out) = call(
            &[
                "compare",
                python.to_str().unwrap(),
                python.to_str().unwrap(),
            ],
            "",
        );
        assert_eq!(
            (code, out.as_str()),
            (Ok(0), "agree: every contract field matches\n")
        );

        let target = dir.join("receipts.jsonl");
        let rows = format!("job-1\t2026-10-22T00:00:00Z\t{GREEN}\n");
        let (code, out) = call(
            &[
                "corpus",
                "--out",
                target.to_str().unwrap(),
                "--forbid",
                "nobody",
            ],
            &rows,
        );
        assert_eq!(code, Ok(0));
        assert!(
            out.starts_with("corpus: 1 rows, 1 shape classes, 1 goldens written to "),
            "{out}"
        );
        let line = std::fs::read_to_string(&target).unwrap();
        let golden = corpus::Golden::from_line(line.trim_end()).unwrap();
        assert_eq!(golden.job_id, "job-1");
        assert_eq!(golden.receipt["head"], "abc");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(call(&["corpus"], "").0.unwrap_err().starts_with("usage:"));
    }
}
