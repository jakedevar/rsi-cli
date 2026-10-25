//! Python-free rolling fingerprint primitives (#363). The canonical callers
//! switch only with the complete lander/QA path; these entry points can also be
//! called in-process by Rust. Hash input bytes remain sweep-report v1 bytes.
use crate::Ctx;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::Command;

pub use crate::wire_json::{legacy_json, sha256};

/// Only stdout participates in version probes. Stderr may contain rustup
/// progress or warnings, and a failed probe must never produce a fingerprint.
pub fn output(repo: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let done = Command::new(program)
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    probe_output(done.status.success(), &done.stdout, &done.stderr, program)
}

fn probe_output(
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
    program: &str,
) -> Result<String, String> {
    if !success {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(stderr).trim()
        ));
    }
    String::from_utf8(stdout.to_vec())
        .map(|s| s.trim().to_owned())
        .map_err(|e| e.to_string())
}

pub fn host_class_from(label: &str, hostname: &str, machine_id: &str) -> Result<String, String> {
    let hostname = hostname.trim().to_lowercase();
    if hostname.is_empty() {
        return Err("QA host has no hostname".into());
    }
    let identity = sha256(format!("{hostname}\0{}", machine_id.trim()));
    Ok(format!("{label}@{hostname}:{}", &identity[..16]))
}

pub fn host_class(ctx: &Ctx) -> Result<String, String> {
    let label = match ctx.var("RSI_QA_HOST_CLASS").filter(|s| !s.is_empty()) {
        Some(label) => label.to_owned(),
        None => {
            #[cfg(unix)]
            let (system, machine) = (
                output(&ctx.cwd, "uname", &["-s"])?,
                output(&ctx.cwd, "uname", &["-m"])?,
            );
            #[cfg(windows)]
            let (system, machine) = (
                "Windows".to_owned(),
                ctx.var("PROCESSOR_ARCHITEW6432")
                    .or_else(|| ctx.var("PROCESSOR_ARCHITECTURE"))
                    .unwrap_or(std::env::consts::ARCH)
                    .to_owned(),
            );
            #[cfg(not(any(unix, windows)))]
            return Err("QA host identity is unsupported on this platform".into());
            #[cfg(any(unix, windows))]
            format!("{}-{}", system.to_lowercase(), machine.to_lowercase())
        }
    };
    let hostname = output(&ctx.cwd, "hostname", &[])?;
    #[cfg(target_os = "linux")]
    let machine_id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
    #[cfg(not(target_os = "linux"))]
    let machine_id = String::new();
    host_class_from(&label, &hostname, &machine_id)
}

/// The injected probe is for behavior tests and remote executors. Production
/// uses `output`, always rooted at the selected checkout, without Python.
pub fn fingerprint_with(
    sha: &str,
    shard: &str,
    jobs: u32,
    host: &str,
    target: Option<&str>,
    mut probe: impl FnMut(&str, &[&str]) -> Result<String, String>,
) -> Result<Value, String> {
    let rustc = probe("rustc", &["-vV"])?;
    let host_target = rustc
        .lines()
        .find_map(|s| s.strip_prefix("host: "))
        .map(str::trim)
        .ok_or("rustc -vV did not report a host target triple")?;
    if host_target.is_empty() {
        return Err("rustc -vV did not report a host target triple".into());
    }
    let mut inputs = json!({
        "rustc": rustc, "cargo": probe("cargo", &["--version"] )?,
        "nextest": probe("cargo", &["nextest", "--version"] )?,
        "target_triple": target.filter(|s| !s.is_empty()).unwrap_or(host_target),
        "host_class": host, "feature": format!("test-shard-{shard}"),
        "jobs": jobs, "test_threads": jobs,
    });
    for (key, path) in [
        ("runner_blob", "scripts/run-rsid-test-shards.sh"),
        ("checker_blob", "scripts/check-rsid-test-shards.py"),
        ("nextest_config_blob", ".config/nextest.toml"),
    ] {
        // Preserve the v1 blob path until #362's full runner transition. Changing
        // it here would invalidate otherwise identical in-flight cache entries.
        inputs[key] = Value::String(probe(
            "git",
            &["rev-parse", "--verify", &format!("{sha}:{path}")],
        )?);
    }
    Ok(json!({"digest": sha256(legacy_json(&inputs, true)), "inputs": inputs}))
}

pub fn fingerprint(ctx: &Ctx, sha: &str, shard: &str, jobs: u32) -> Result<Value, String> {
    fingerprint_with(
        sha,
        shard,
        jobs,
        &host_class(ctx)?,
        ctx.var("CARGO_BUILD_TARGET"),
        |p, a| output(&ctx.cwd, p, a),
    )
}

pub fn lowercase_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn valid_shard(s: &str) -> bool {
    s.rsplit_once('-').is_some_and(|(prefix, digits)| {
        ["store", "session", "memory", "other"].contains(&prefix)
            && digits.len() == 2
            && digits.bytes().all(|c| c.is_ascii_digit())
    })
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parse = || -> Result<(&str, &str, u32, bool), String> {
        let (mut sha, mut shard, mut jobs, mut json) = (None, None, None, false);
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--json" => json = true,
                "--sha" => sha = Some(args.next().ok_or("--sha requires a value")?.as_str()),
                "--shard" => shard = Some(args.next().ok_or("--shard requires a value")?.as_str()),
                "--jobs" => {
                    jobs = Some(
                        args.next()
                            .ok_or("--jobs requires a value")?
                            .parse::<u32>()
                            .map_err(|_| "--jobs must be 1..64")?,
                    )
                }
                _ => return Err(format!("unrecognized argument: {arg}")),
            }
        }
        let sha = sha.ok_or("--sha is required")?;
        let shard = shard.ok_or("--shard is required")?;
        let jobs = jobs.ok_or("--jobs is required")?;
        if !lowercase_hex(sha, 40) {
            return Err("--sha must be a full lowercase commit ID".into());
        }
        if !valid_shard(shard) {
            return Err("--shard must be a library shard name".into());
        }
        if !(1..=64).contains(&jobs) {
            return Err("--jobs must be 1..64".into());
        }
        Ok((sha, shard, jobs, json))
    };
    let (sha, shard, jobs, json) = match parse() {
        Ok(parsed) => parsed,
        Err(e) => {
            let _ = writeln!(err, "rolling-shard-fingerprint: {e}");
            return 2;
        }
    };
    match fingerprint(ctx, sha, shard, jobs) {
        Ok(value) => {
            let _ = writeln!(
                out,
                "{}",
                if json {
                    legacy_json(&value, false)
                } else {
                    format!("sha256:{}", value["digest"].as_str().unwrap_or_default())
                }
            );
            0
        }
        Err(e) => {
            let _ = writeln!(err, "rolling-shard-fingerprint: {e}");
            1
        }
    }
}

#[cfg(test)]
#[path = "qa_fingerprint_tests.rs"]
mod tests;
