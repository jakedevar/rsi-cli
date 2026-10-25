//! Staged Python-free scoped-test components (#423). Canonical callers must
//! remain on their existing contract until all mandatory lane ports integrate.
pub mod admission;
pub mod baseline;
pub mod batch;
pub mod contracts;
pub mod converged;
pub mod derivation;
pub mod execution;
pub mod failure_snapshot;
pub mod gitx;
pub mod guards;
pub mod host;
pub mod linux_check;
pub mod options;
pub mod orchestration;
pub mod persistence;
pub mod planner;
pub mod process;
pub mod provenance;
pub mod provision;
pub mod receipt;
pub mod residual;
pub mod residual_run;
pub mod runner;
pub mod scratch;
pub mod selection;
pub mod warnings;

use crate::Ctx;
use serde_json::json;
use std::io::{self, Write};
use std::path::Path;

/// This inventory is explicit so a missing helper cannot silently become an
/// empty lane. The owning slices supply the final wrapper/API names at switch.
pub const DEPENDENCIES: &[(&str, &str)] = &[
    ("#426", "scripts/check-touched-shards"),
    ("#361", "tools/check-released-migrations.py"),
    ("#362", "scripts/check-rsid-test-shards.py"),
    ("#362", "scripts/check-target-reclaim.py"),
    ("#362", "scripts/check-guard-regressions.py"),
    ("#362", "scripts/check-store-boundary.py"),
    ("#362", "scripts/recent-landing-check.py"),
    ("#362", "scripts/scoped-test-failure.py"),
    ("#363", "scripts/desktop-test-gate.py"),
    ("#366", "scripts/python-test-gate.py"),
];

pub fn dependency_state(root: &Path) -> Vec<serde_json::Value> {
    DEPENDENCIES
        .iter()
        .map(|(owner, path)| {
            let status = match std::fs::read(root.join(path)) {
                Ok(bytes)
                    if path.ends_with(".py")
                        || bytes
                            .split(|b| *b == b'\n')
                            .next()
                            .is_some_and(|line| line.windows(6).any(|w| w == b"python")) =>
                {
                    "requires_port"
                }
                Ok(_) => "requires_contract_integration",
                Err(_) => "replacement_contract_required",
            };
            json!({"owner":owner,"path":path,"status":status})
        })
        .collect()
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = (|| -> io::Result<i32> {
        if args == ["--hold-admission"] {
            admission::hold(ctx, io::stdin().lock(), out)?;
            return Ok(0);
        }
        if args.len() == 2 && args[0] == "--execute-plan" {
            if ctx.var("RSI_CARGO_SLOT_HELD") != Some("1") {
                return Err(io::Error::other(
                    "package execution requires a held cargo-slot admission",
                ));
            }
            let cancel = process::install_signals()?;
            return execution::main(&ctx.path(&args[1]), ctx, &cancel);
        }
        // This one binary is its own content-verified helper: the PID-isolation canary
        // and reaping init, and the killable renumber-tool step of the converged contracts.
        if let Some((_, entry, _)) = args.first().and_then(|name| {
            ["pid-isolate", "rolling-migration-renumber", "new-python"]
                .contains(&name.as_str())
                .then(|| {
                    crate::GUARDS
                        .iter()
                        .find(|(guard, _, _)| *guard == name.as_str())
                })
                .flatten()
        }) {
            return Ok(entry(&args[1..], ctx, out, err));
        }
        let contracts = converged::Converged::new(ctx, std::env::current_exe()?)?;
        Ok(runner::run(args, ctx, out, err, contracts))
    })();
    match result {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "scoped-test: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_dependencies_are_red_even_for_explicit_or_dry_run_requests() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path(), &[]);
        for args in [
            vec!["--dry-run".into()],
            vec!["--filter".into(), "rsi=test(=one)".into()],
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert_eq!(main(&args, &ctx, &mut out, &mut err), 2);
            let text = String::from_utf8(out).unwrap();
            let receipt: serde_json::Value =
                serde_json::from_str(text.trim().strip_prefix("SCOPED_TEST_RECEIPT ").unwrap())
                    .unwrap();
            assert_eq!(receipt["ok"], false);
            assert_eq!(receipt["failure_class"], "runner_unavailable");
            assert_eq!(
                receipt["dependencies"].as_array().unwrap().len(),
                DEPENDENCIES.len()
            );
        }
    }
    #[test]
    fn package_entry_requires_admission_before_opening_a_plan() {
        let ctx = Ctx::new(".", &[]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            main(
                &["--execute-plan".into(), "missing".into()],
                &ctx,
                &mut out,
                &mut err
            ),
            2
        );
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("requires a held cargo-slot admission")
        );
    }
}
