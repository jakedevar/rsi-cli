//! Command line of the native scoped-test, keeping the retained flags/defaults.
use crate::cli::{self, Opt};
use std::io::Write;

pub const FILTER_BUDGET: usize = 12;
pub const TEST_TIME_BOX_SEC: f64 = 600.0;
pub const USAGE: &str = "[--base REF] [--head REF] [--repo DIR] [--filter PACKAGE=FILTER]... \
[--runtime-max-sec N] [--total-max-sec N] [--slot-wait-max-sec N] [--job-max-sec N] \
[--filter-budget N] [--test-time-box-sec N] [--runner auto|nextest|libtest] [--cpu-quota N] \
[--test-shards N] [--reclaim-incremental] [--tmpfs] [--dry-run] [--warning-baseline-from LOG]";

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub base: String,
    pub head: String,
    pub repo: String,
    /// `None` derives the plan from the diff; `Some` replaces the derivation.
    pub filters: Option<Vec<String>>,
    pub runtime_max: f64,
    pub total_max: Option<f64>,
    pub slot_wait_max: Option<f64>,
    pub job_max: Option<f64>,
    pub filter_budget: usize,
    pub test_time_box: f64,
    pub runner: String,
    pub cargo_slot: Option<String>,
    pub cpu_quota: f64,
    pub test_shards: Option<usize>,
    pub reclaim_incremental: bool,
    pub tmpfs: bool,
    pub dry_run: bool,
    pub warning_baseline_from: Option<String>,
}

fn positive(name: &str, text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(number) if number.is_finite() && number > 0.0 => Ok(number),
        _ => Err(format!("argument {name}: must be positive and finite")),
    }
}

const OPTS: &[Opt] = &[
    Opt {
        name: "--base",
        value: true,
    },
    Opt {
        name: "--head",
        value: true,
    },
    Opt {
        name: "--repo",
        value: true,
    },
    Opt {
        name: "--runtime-max-sec",
        value: true,
    },
    Opt {
        name: "--total-max-sec",
        value: true,
    },
    Opt {
        name: "--slot-wait-max-sec",
        value: true,
    },
    Opt {
        name: "--job-max-sec",
        value: true,
    },
    Opt {
        name: "--filter-budget",
        value: true,
    },
    Opt {
        name: "--test-time-box-sec",
        value: true,
    },
    Opt {
        name: "--runner",
        value: true,
    },
    Opt {
        name: "--cargo-slot",
        value: true,
    },
    Opt {
        name: "--cpu-quota",
        value: true,
    },
    Opt {
        name: "--test-shards",
        value: true,
    },
    Opt {
        name: "--reclaim-incremental",
        value: false,
    },
    Opt {
        name: "--tmpfs",
        value: false,
    },
    Opt {
        name: "--dry-run",
        value: false,
    },
    Opt {
        name: "--warning-baseline-from",
        value: true,
    },
];

/// Pull every `--filter`/`--test-filter` (repeatable) out of `args`.
fn take_filters(args: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut filters = Vec::new();
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        let name = arg.split('=').next().unwrap_or("");
        if name == "--filter" || name == "--test-filter" {
            if let Some((_, value)) = arg.split_once('=') {
                filters.push(value.to_string());
            } else if let Some(value) = args.get(index).filter(|v| !v.starts_with('-')) {
                filters.push(value.clone());
                index += 1;
            } else {
                return Err(format!("argument {name}: expected one argument"));
            }
        } else {
            rest.push(arg.clone());
        }
    }
    Ok((filters, rest))
}

/// `Err(code)`: usage was printed (2) or help requested (0).
pub fn parse(
    args: &[String],
    environment_filters: Option<&str>,
    err: &mut dyn Write,
) -> Result<Options, i32> {
    let usage_error = |err: &mut dyn Write, message: &str| {
        let _ = writeln!(err, "usage: scoped-test {USAGE}");
        let _ = writeln!(err, "scoped-test: error: {message}");
        2
    };
    let (filters, rest) = match take_filters(args) {
        Ok(split) => split,
        Err(message) => return Err(usage_error(err, &message)),
    };
    let parsed = match cli::parse("scoped-test", USAGE, &rest, OPTS, &[], Some(0), err) {
        Ok(parsed) => parsed,
        Err(exit) => return Err(exit.code()),
    };
    let number = |name: &'static str| -> Result<Option<f64>, String> {
        parsed.value(name).map(|v| positive(name, v)).transpose()
    };
    let build = || -> Result<Options, String> {
        let runner = parsed.value("--runner").unwrap_or("auto").to_string();
        if !["auto", "nextest", "libtest"].contains(&runner.as_str()) {
            return Err(format!(
                "argument --runner: invalid choice: {runner:?} (choose from auto, nextest, libtest)"
            ));
        }
        let filter_budget = match parsed.value("--filter-budget") {
            Some(v) => v
                .parse::<usize>()
                .map_err(|_| format!("argument --filter-budget: invalid int value: {v:?}"))?,
            None => FILTER_BUDGET,
        };
        let test_time_box = match parsed.value("--test-time-box-sec") {
            Some(v) => v
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
                .ok_or_else(|| {
                    format!("argument --test-time-box-sec: invalid float value: {v:?}")
                })?,
            None => TEST_TIME_BOX_SEC,
        };
        let test_shards = match parsed.value("--test-shards") {
            Some(v) => Some(
                positive("--test-shards", v)
                    .and_then(|n| {
                        if n.fract() == 0.0 {
                            Ok(n as usize)
                        } else {
                            Err("argument --test-shards: must be a whole number".to_string())
                        }
                    })?
                    .max(1),
            ),
            None => None,
        };
        let filters = if !filters.is_empty() {
            Some(filters)
        } else {
            environment_filters
                .map(|text| {
                    text.split('\n')
                        .filter(|line| !line.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .filter(|lines| !lines.is_empty())
        };
        Ok(Options {
            base: parsed.value("--base").unwrap_or("origin/rolling").into(),
            head: parsed.value("--head").unwrap_or("HEAD").into(),
            repo: parsed.value("--repo").unwrap_or(".").into(),
            filters,
            runtime_max: number("--runtime-max-sec")?.unwrap_or(1800.0),
            total_max: number("--total-max-sec")?,
            slot_wait_max: number("--slot-wait-max-sec")?,
            job_max: number("--job-max-sec")?,
            filter_budget,
            test_time_box,
            runner,
            cargo_slot: parsed.value("--cargo-slot").map(Into::into),
            cpu_quota: number("--cpu-quota")?.unwrap_or(200.0),
            test_shards,
            reclaim_incremental: parsed.flag("--reclaim-incremental"),
            tmpfs: parsed.flag("--tmpfs"),
            dry_run: parsed.flag("--dry-run"),
            warning_baseline_from: parsed.value("--warning-baseline-from").map(Into::into),
        })
    };
    build().map_err(|message| usage_error(err, &message))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn defaults_and_repeatable_filters_match_the_retained_command_line() {
        let mut err = Vec::new();
        let options = parse(&[], None, &mut err).unwrap();
        assert_eq!(options.base, "origin/rolling");
        assert_eq!(options.head, "HEAD");
        assert_eq!(options.repo, ".");
        assert_eq!(options.filters, None);
        assert_eq!(options.runtime_max, 1800.0);
        assert_eq!(options.cpu_quota, 200.0);
        assert_eq!(options.filter_budget, FILTER_BUDGET);
        assert_eq!(options.test_time_box, TEST_TIME_BOX_SEC);
        let options = parse(
            &args(&[
                "--filter",
                "rsi=a",
                "--test-filter=rsid=b",
                "--dry-run",
                "--base=x",
            ]),
            Some("ignored=when\nexplicit=wins"),
            &mut err,
        )
        .unwrap();
        assert_eq!(options.filters, Some(args(&["rsi=a", "rsid=b"])));
        assert!(options.dry_run);
        assert_eq!(options.base, "x");
    }
    #[test]
    fn environment_filters_apply_only_without_flags() {
        let mut err = Vec::new();
        let options = parse(&[], Some("rsi=a\n\nrsid=b\n"), &mut err).unwrap();
        assert_eq!(options.filters, Some(args(&["rsi=a", "rsid=b"])));
        assert_eq!(parse(&[], Some("\n"), &mut err).unwrap().filters, None);
    }
    #[test]
    fn invalid_values_are_usage_errors() {
        for bad in [
            args(&["--runtime-max-sec", "0"]),
            args(&["--cpu-quota=nan"]),
            args(&["--runner", "other"]),
            args(&["--filter-budget", "-1"]),
            args(&["--filter"]),
            args(&["stray"]),
        ] {
            let mut err = Vec::new();
            assert_eq!(parse(&bad, None, &mut err), Err(2), "{bad:?}");
            assert!(!err.is_empty());
        }
    }
}
