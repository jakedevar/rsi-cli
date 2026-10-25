//! Command lines of the `target-pool` subcommands beyond `seed` and `key` (#453):
//! `build`, `seed-from`, `gc`, `reclaim-idle`, `refresh`, `status` and `summary`,
//! parsed like the Python tool's argparse and handed to `target_pool_ops` /
//! `target_pool_build`. A command returns its exit code; a usage error prints
//! argparse's `usage:`/`error:` pair and returns 2.
use crate::target_pool::{DEFAULT_MAX_DISTANCE, Env, Git, self_program};
use crate::target_pool_build::{BuildOptions, SeedFromOptions, request_build};
use crate::target_pool_ops::{
    self, DEFAULT_RECLAIM_INTERVAL, DEFAULT_REF, GcCommand, RefreshOptions, default_keep,
    default_min_free_gib,
};
use crate::{Ctx, cli};
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

fn opts(names: &[(&'static str, bool)]) -> Vec<cli::Opt> {
    names
        .iter()
        .map(|(name, value)| cli::Opt { name, value: *value })
        .collect()
}

/// A numeric option: its parsed value, the default, or a usage error (`None`).
fn number<T: FromStr>(
    parsed: &cli::Parsed,
    name: &'static str,
    default: T,
    prog: &str,
    usage: &str,
    err: &mut dyn Write,
) -> Option<T> {
    match parsed.value(name) {
        None => Some(default),
        Some(text) => match text.trim().parse::<T>() {
            Ok(value) => Some(value),
            Err(_) => {
                let _ = writeln!(err, "usage: {prog} {usage}");
                let _ = writeln!(
                    err,
                    "{prog}: error: argument {name}: invalid value: {text:?}"
                );
                None
            }
        },
    }
}

/// Parse or fail with exit code 2 (the usage text is already printed).
macro_rules! parse_or_exit {
    ($prog:expr, $usage:expr, $args:expr, $names:expr, $max:expr, $err:expr) => {
        match cli::parse($prog, $usage, $args, &opts($names), &[], $max, $err) {
            Ok(parsed) => parsed,
            Err(exit) => return Ok(exit.code()),
        }
    };
}

macro_rules! num_or_exit {
    ($parsed:expr, $name:expr, $default:expr, $prog:expr, $usage:expr, $err:expr) => {
        match number(&$parsed, $name, $default, $prog, $usage, $err) {
            Some(value) => value,
            None => return Ok(2),
        }
    };
}

pub fn build(args: &[String], ctx: &Ctx, env: &Env, err: &mut dyn Write) -> io::Result<i32> {
    const USAGE: &str = "--repo DIR [--sha SHA] [--keep N] [--min-free-gib G] [--retry-after SEC] [--max-distance N]";
    let parsed = parse_or_exit!(
        "target-pool build",
        USAGE,
        args,
        &[
            ("--repo", true),
            ("--sha", true),
            ("--keep", true),
            ("--min-free-gib", true),
            ("--retry-after", true),
            ("--max-distance", true)
        ],
        Some(0),
        err
    );
    let Some(repo) = parsed.value("--repo") else {
        let _ = writeln!(err, "usage: target-pool build {USAGE}");
        let _ = writeln!(err, "target-pool build: error: the following arguments are required: --repo");
        return Ok(2);
    };
    let prog = "target-pool build";
    let options = BuildOptions {
        repo: ctx.path(repo),
        sha: parsed.value("--sha").map(str::to_string),
        keep: num_or_exit!(parsed, "--keep", default_keep(env), prog, USAGE, err),
        min_free_gib: num_or_exit!(parsed, "--min-free-gib", default_min_free_gib(env)?, prog, USAGE, err),
        retry_after: num_or_exit!(parsed, "--retry-after", 3600.0, prog, USAGE, err),
        max_distance: num_or_exit!(parsed, "--max-distance", DEFAULT_MAX_DISTANCE, prog, USAGE, err),
    };
    crate::target_pool_build::build(&options, env, err)
}

pub fn seed_from(
    args: &[String],
    ctx: &Ctx,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<i32> {
    const USAGE: &str = "HOST [--repo DIR] [--sha SHA] [--dry-run] [--keep N] [--level N] [--remote-tool CMD] [--min-free-gib G] [--min-headroom-gib G]";
    let prog = "target-pool seed-from";
    let parsed = parse_or_exit!(
        prog,
        USAGE,
        args,
        &[
            ("--repo", true),
            ("--sha", true),
            ("--dry-run", false),
            ("--keep", true),
            ("--level", true),
            ("--remote-tool", true),
            ("--min-free-gib", true),
            ("--min-headroom-gib", true)
        ],
        Some(1),
        err
    );
    let Some(host) = parsed.positionals.first() else {
        let _ = writeln!(err, "usage: {prog} {USAGE}");
        let _ = writeln!(err, "{prog}: error: the following arguments are required: host");
        return Ok(2);
    };
    let keep = match parsed.value("--keep") {
        None => None,
        Some(_) => Some(num_or_exit!(parsed, "--keep", 0i64, prog, USAGE, err)),
    };
    let options = SeedFromOptions {
        host: host.clone(),
        repo: ctx.path(parsed.value("--repo").unwrap_or(".")),
        sha: parsed.value("--sha").map(str::to_string),
        dry_run: parsed.flag("--dry-run"),
        keep,
        level: num_or_exit!(parsed, "--level", 3i64, prog, USAGE, err),
        remote_tool: parsed
            .value("--remote-tool")
            .unwrap_or(".rsi/bin/target-pool")
            .to_string(),
        min_free_gib: num_or_exit!(parsed, "--min-free-gib", default_min_free_gib(env)?, prog, USAGE, err),
        min_headroom_gib: num_or_exit!(parsed, "--min-headroom-gib", 5.0, prog, USAGE, err),
    };
    crate::target_pool_build::seed_from(&options, env, out, err)
}

pub fn gc(
    args: &[String],
    ctx: &Ctx,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<i32> {
    const USAGE: &str = "[--repo DIR] [--keep N] [--pressure] [--json] [--ref REF] [--max-distance N]";
    let prog = "target-pool gc";
    let parsed = parse_or_exit!(
        prog,
        USAGE,
        args,
        &[
            ("--repo", true),
            ("--keep", true),
            ("--pressure", false),
            ("--json", false),
            ("--ref", true),
            ("--max-distance", true)
        ],
        Some(0),
        err
    );
    let repo = parsed.value("--repo").map(|r| std::fs::canonicalize(ctx.path(r))).transpose()?;
    let keep = match parsed.value("--keep") {
        None => None,
        Some(_) => Some(num_or_exit!(parsed, "--keep", 0i64, prog, USAGE, err)),
    };
    let options = GcCommand {
        repo: repo.as_deref(),
        keep,
        pressure: parsed.flag("--pressure"),
        json: parsed.flag("--json"),
        reference: Some(parsed.value("--ref").unwrap_or(DEFAULT_REF)),
        max_distance: num_or_exit!(parsed, "--max-distance", DEFAULT_MAX_DISTANCE, prog, USAGE, err),
    };
    target_pool_ops::gc_command(&options, env, out)
}

pub fn reclaim_idle(
    args: &[String],
    ctx: &Ctx,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<i32> {
    const USAGE: &str = "[--min-interval SEC] [--filesystem PATH]";
    let prog = "target-pool reclaim-idle";
    let parsed = parse_or_exit!(
        prog,
        USAGE,
        args,
        &[("--min-interval", true), ("--filesystem", true)],
        Some(0),
        err
    );
    let min_interval = num_or_exit!(parsed, "--min-interval", DEFAULT_RECLAIM_INTERVAL, prog, USAGE, err);
    let filesystem = parsed.value("--filesystem").map(|p| ctx.path(p));
    target_pool_ops::reclaim_idle_command(min_interval, filesystem.as_deref(), env, out)
}

pub fn refresh(
    args: &[String],
    ctx: &Ctx,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<i32> {
    const USAGE: &str = "[--repo DIR] [--ref REF] [--max-distance N] [--retry-after SEC]";
    let prog = "target-pool refresh";
    let parsed = parse_or_exit!(
        prog,
        USAGE,
        args,
        &[
            ("--repo", true),
            ("--ref", true),
            ("--max-distance", true),
            ("--retry-after", true)
        ],
        Some(0),
        err
    );
    let repo: Option<PathBuf> = parsed.value("--repo").map(|r| std::fs::canonicalize(ctx.path(r))).transpose()?;
    let options = RefreshOptions {
        repo: repo.as_deref(),
        reference: parsed.value("--ref").unwrap_or(DEFAULT_REF),
        max_distance: num_or_exit!(parsed, "--max-distance", DEFAULT_MAX_DISTANCE, prog, USAGE, err),
        retry_after: num_or_exit!(parsed, "--retry-after", 3600.0, prog, USAGE, err),
    };
    let program = self_program()?;
    let launch = |git: &Git, repo: &std::path::Path, tip: &str| request_build(git, repo, tip, &program);
    target_pool_ops::refresh_command(&options, env, &launch, out)
}

pub fn status(
    args: &[String],
    ctx: &Ctx,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<i32> {
    let parsed = parse_or_exit!("target-pool status", "[--repo DIR]", args, &[("--repo", true)], Some(0), err);
    let repo = parsed.value("--repo").map(|r| std::fs::canonicalize(ctx.path(r))).transpose()?;
    target_pool_ops::status_command(repo.as_deref(), env, out)
}

pub fn summary(
    args: &[String],
    ctx: &Ctx,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<i32> {
    let parsed = parse_or_exit!(
        "target-pool summary",
        "[--repo DIR] [--ref REF] [--filesystem PATH]",
        args,
        &[("--repo", true), ("--ref", true), ("--filesystem", true)],
        Some(0),
        err
    );
    let repo = parsed.value("--repo").map(|r| std::fs::canonicalize(ctx.path(r))).transpose()?;
    let filesystem = parsed.value("--filesystem").map(|p| ctx.path(p));
    target_pool_ops::summary_command(
        repo.as_deref(),
        parsed.value("--ref").unwrap_or("origin/rolling"),
        filesystem.as_deref(),
        env,
        out,
    )
}
