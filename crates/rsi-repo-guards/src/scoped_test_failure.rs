//! Conservative interruption classification; a classification never makes a red green.
use crate::{Ctx, re::Re};
use serde_json::Value;
use std::{fs, io::Write, path::Path};

pub fn signal_only<'a>(lines: impl IntoIterator<Item = &'a str>) -> bool {
    let ansi = Re::new(r"\x1b\[[0-9;]*m");
    let signal = Re::new(r"\bsignal:\s*(?:2|9|15)\b|^SCOPED_PROCESS_SIGNAL SIG(?:INT|KILL|TERM)$");
    let compiler = Re::new(r"^\s*error(?:\[E\d+\])?:");
    let summary = Re::new(r"^\s*error: (?:could not compile `|test failed, to rerun pass )");
    let failure = Re::new(
        r"^SCOPED_(?:FAILURE |TEST_EXIT )|^\s*(?:test .* \.\.\. |test result: )FAILED\b|^\s*FAILED\s*$|^\s*(?:FAIL|TIMEOUT|ABORT)\s+\[",
    );
    let previous = Re::new(r"due to \d+ previous errors?");
    let exit = Re::new(r"\bexit status: [1-9]\d*\b");
    let mut interrupted = false;
    for raw in lines {
        let line = ansi.sub(raw, "");
        let line = line.trim_end();
        if (compiler.is_match(line) && !summary.is_match(line))
            || failure.is_match(line)
            || previous.is_match(line)
            || exit.is_match(line)
        {
            return false;
        }
        interrupted |= signal.is_match(line);
    }
    interrupted
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}

/// Reads full logs. Missing evidence and every unrelated failure veto the classification.
pub fn infrastructure_interrupted(receipt: &Value) -> bool {
    if [
        "dry_run",
        "failures",
        "unmatched_filters",
        "derive_failure",
        "ok_reason",
    ]
    .iter()
    .any(|k| truthy(&receipt[k]))
        || matches!(
            receipt["warnings"]["status"].as_str(),
            Some("new_warnings" | "baseline_raised" | "unavailable")
        )
        || receipt["static_guards"].as_object().is_some_and(|g| {
            g.values()
                .any(|v| !matches!(v["status"].as_str(), Some("ok" | "skipped")))
        })
        || ["python", "desktop"].iter().any(|k| {
            receipt[k]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v["status"] != "passed"))
        })
    {
        return false;
    }
    let mut failed = false;
    for package in receipt["packages"].as_array().into_iter().flatten() {
        if matches!(
            package["status"].as_str(),
            Some("passed" | "not applicable")
        ) {
            continue;
        }
        if package["status"] != "failed" || truthy(&package["failed_tests"]) {
            return false;
        }
        let (Some(dir), Some(name)) = (receipt["log_dir"].as_str(), package["package"].as_str())
        else {
            return false;
        };
        if dir.is_empty() {
            return false;
        }
        let Ok(log) = fs::read(Path::new(dir).join(format!("{name}.log"))) else {
            return false;
        };
        if !signal_only(String::from_utf8_lossy(&log).lines()) {
            return false;
        }
        failed = true;
    }
    failed
}

/// Shell compatibility: one receipt JSON path, prints a JSON boolean (exit 0).
/// Invalid/missing input exits 2, never a false success receipt.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.len() != 1 {
        let _ = writeln!(err, "usage: scoped-test-failure RECEIPT.json");
        return 2;
    }
    let result = fs::read(ctx.path(&args[0]))
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_slice::<Value>(&s).map_err(|e| e.to_string()));
    match result {
        Ok(r) => {
            let _ = writeln!(out, "{}", infrastructure_interrupted(&r));
            0
        }
        Err(e) => {
            let _ = writeln!(err, "scoped-test-failure: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::repo_root;
    use serde_json::json;
    #[test]
    fn ported_signal_and_failure_fixtures() {
        let fixture = |name| {
            fs::read_to_string(repo_root().join(format!(
                "scripts/tests/fixtures/scoped-test-failure/{name}.txt"
            )))
            .unwrap()
        };
        assert!(signal_only(fixture("signal-only").lines()));
        assert!(!signal_only(fixture("signal-and-compiler-error").lines()));
        for prefix in [String::new(), fixture("signal-only")] {
            assert!(!signal_only(
                format!("{prefix}{}", fixture("test-failure")).lines()
            ));
        }
        for (sig, name) in [(2, "SIGINT"), (9, "SIGKILL"), (15, "SIGTERM")] {
            for log in [
                format!("process didn't exit successfully (signal: {sig})"),
                format!("SCOPED_PROCESS_SIGNAL {name}"),
            ] {
                assert!(signal_only([log.as_str()]));
                for error in [
                    "error: linking failed",
                    "\x1b[31merror[E0308]: bad type\x1b[0m",
                    "SCOPED_TEST_EXIT 1",
                    "process exited (exit status: 1)",
                ] {
                    assert!(!signal_only([error, log.as_str()]));
                }
            }
        }
        assert!(!signal_only(["process exited (signal: 11, SIGSEGV)"]));
    }
    #[test]
    fn ported_receipt_vetoes_and_full_log_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("rsid.log");
        fs::write(&log, "SCOPED_PROCESS_SIGNAL SIGTERM\n").unwrap();
        let base = json!({"log_dir":tmp.path(), "head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "packages":[{"package":"rsid","status":"failed","completed":0}]});
        assert!(infrastructure_interrupted(&base));
        for extra in [
            json!({"failures":[{"test":"tests::failed"}]}),
            json!({"static_guards":{"whitespace":{"status":"fail"}}}),
            json!({"python":[{"status":"failed"}]}),
            json!({"desktop":[{"status":"failed"}]}),
            json!({"warnings":{"status":"new_warnings"}}),
            json!({"unmatched_filters":["rsid=missing"]}),
            json!({"packages":[{"package":"other","status":"running"}]}),
            json!({"packages":[{"package":"rsid","status":"timed out"}]}),
            json!({"dry_run":true}),
            json!({"derive_failure":"bad"}),
            json!({"ok_reason":"no tests"}),
        ] {
            let mut r = base.clone();
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(!infrastructure_interrupted(&r), "{extra}");
        }
        fs::write(
            &log,
            format!(
                "error: linking failed\n{}SCOPED_PROCESS_SIGNAL SIGTERM\n",
                "noise\n".repeat(10000)
            ),
        )
        .unwrap();
        assert!(!infrastructure_interrupted(&base));
        fs::remove_file(log).unwrap();
        assert!(!infrastructure_interrupted(&base));
        assert!(!infrastructure_interrupted(&json!({"packages":[]})));
    }
}
