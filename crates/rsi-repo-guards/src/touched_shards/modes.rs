//! Audits and the execution modes of the derivation: the cheap git/policy audits,
//! the supervised shard compile (`--compile`), the landing-filter run
//! (`--run-filters`) and receipt files. Planning (`--filters-only`) never reaches
//! the cargo paths. Receipts are byte-compatible with the legacy writer:
//! 2-space indentation, ASCII-escaped, written atomically.
use super::derive::Loaded;
use crate::{Ctx, re::Re, released_migrations, rsid_test_shards};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, Ordering},
};
use std::time::{Duration, Instant};

pub const STALE_HINT: &str = "every shard compile failed with the same error while the tree may compile directly: a stale workspace-package artifact in a reused target is suspected; retry with CHECK_TOUCHED_SHARDS_CLEAN=1 (cleans only a target under the repo) or point CARGO_TARGET_DIR at a private directory (#1260)";

fn tail(text: &str, count: usize) -> String {
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    chars[chars.len().saturating_sub(count)..].iter().collect()
}

/// `check-rsid-test-shards --require-gates`, in process (no interpreter).
pub fn shard_gate_audit(loaded: &Loaded) -> Value {
    if !loaded.repo.root.join("crates/rsid/Cargo.toml").exists() {
        return json!({"verdict": "skipped", "detail": "script absent"});
    }
    let ctx = Ctx::new(&loaded.repo.root, &[]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let root = loaded.repo.root.to_string_lossy().into_owned();
    let code = rsid_test_shards::main(
        &["--require-gates".into(), "--root".into(), root],
        &ctx,
        &mut out,
        &mut err,
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    super::derive::verdict_value(code == 0, &tail(&text, 300))
}

/// `git diff --check`: whitespace errors in the candidate's own changes.
pub fn diff_check(loaded: &Loaded, changed: &[String], multiple_bases: bool) -> Value {
    let ctx = &loaded.ctx;
    let mut args: Vec<String> = vec!["diff".into(), "--check".into()];
    if multiple_bases && !changed.is_empty() {
        args.extend([ctx.base.clone(), ctx.head.clone(), "--".into()]);
        args.extend(changed.iter().cloned());
    } else {
        args.push(format!("{}...{}", ctx.base, ctx.head));
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match loaded.repo.run(&refs) {
        Ok(output) => super::derive::verdict_value(
            output.status.success(),
            &tail(&String::from_utf8_lossy(&output.stdout), 300),
        ),
        Err(error) => super::derive::verdict_value(false, &error.to_string()),
    }
}

/// The append-only released-migration policy (#361), in process.
pub fn released_migrations_audit(loaded: &Loaded) -> Value {
    if loaded.head_blob(released_migrations::MANIFEST).is_none() {
        return json!({"verdict": "skipped", "detail": "tool absent"});
    }
    let ctx = Ctx::new(&loaded.repo.root, &[]);
    match released_migrations::check(&ctx, &loaded.ctx.audit_base, &loaded.ctx.head) {
        Ok(message) => super::derive::verdict_value(true, &tail(&message, 300)),
        Err(message) => super::derive::verdict_value(false, &tail(&message, 300)),
    }
}

/// Receipt JSON as the legacy writer printed it: `ensure_ascii`, 2-space indent.
pub fn python_json(value: &Value, indent: bool) -> String {
    let text = if indent {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// Write `receipt` atomically so a killed run never leaves half a file.
pub fn write_receipt(path: &Path, receipt: &Value) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let scratch = path.with_file_name(format!("{name}.partial"));
    std::fs::write(&scratch, format!("{}\n", python_json(receipt, true)))?;
    std::fs::rename(scratch, path)
}

pub fn cargo_prefix(ctx: &Ctx) -> Vec<String> {
    if let Some(over) = ctx.var("CHECK_TOUCHED_SHARDS_CARGO") {
        return over.split_whitespace().map(str::to_owned).collect();
    }
    let home = ctx
        .var("HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from));
    let slot = home
        .map(|h| h.join(".rsi/bin/cargo-slot"))
        .filter(|p| p.exists());
    // An outer cargo-slot already holds a governor slot and marks its child;
    // acquiring again would reserve two slots.
    let held = ctx
        .var("RSI_CARGO_SLOT_HELD")
        .is_some_and(|v| !v.is_empty());
    let mut prefix: Vec<String> = match (&slot, held) {
        (Some(slot), false) => vec![slot.to_string_lossy().into_owned()],
        _ => Vec::new(),
    };
    prefix.extend(["env", "-u", "RSI_PROCESS_OWNERSHIP_NAMESPACE", "cargo"].map(str::to_owned));
    prefix
}

/// Seconds a cargo-slot wrapper reports it queued for a build slot.
pub fn slot_wait_seconds(stderr: &str) -> u64 {
    Re::new(r"(?m)^rsi-phase \d+ slot-granted: waited (\d+)s")
        .findall1(stderr)
        .iter()
        .filter_map(|n| n.parse::<u64>().ok())
        .sum()
}

/// The distinct `error...` lines of a cargo run, for comparing failures across shards.
pub fn error_signature(stderr: &str) -> String {
    let lines: std::collections::BTreeSet<_> = stderr
        .lines()
        .filter(|l| l.starts_with("error") && !l.contains("could not compile"))
        .map(str::trim)
        .collect();
    lines.into_iter().collect::<Vec<_>>().join("\n")
}

/// The first error blocks and spans (40 lines/4000 chars) ahead of the tail.
pub fn failure_excerpt(stderr: &str) -> String {
    let error = Re::new(r"^error(?:\[[^\]]+\])?:");
    let warning = Re::new(r"^warning(?:\[[^\]]+\])?:");
    let mut diagnostics = Vec::new();
    let mut in_error = false;
    for line in stderr.lines() {
        if error.is_match(line) {
            in_error = true;
        } else if warning.is_match(line) {
            in_error = false;
        }
        if in_error {
            diagnostics.push(line);
            if diagnostics.len() >= 40 {
                break;
            }
        }
    }
    let joined = diagnostics.join("\n");
    let first: String = joined
        .chars()
        .take(4000)
        .collect::<String>()
        .trim_end()
        .to_owned();
    let chars: Vec<char> = stderr.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(2000)..].iter().collect();
    if first.is_empty() {
        tail
    } else {
        format!("{first}\n\nstderr tail:\n{tail}")
    }
}

pub struct Captured {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run to completion, or kill the child and return `None` when `interrupt` is set.
pub fn run_cancellable(
    command: &[String],
    cwd: &Path,
    env: &[(String, String)],
    interrupt: &AtomicBool,
) -> std::io::Result<Option<Captured>> {
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let read_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = out.as_mut() {
            let _ = std::io::Read::read_to_end(out, &mut buf);
        }
        buf
    });
    let read_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(err) = err.as_mut() {
            let _ = std::io::Read::read_to_end(err, &mut buf);
        }
        buf
    });
    loop {
        if interrupt.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        if let Some(status) = child.try_wait()? {
            let stdout = read_out.join().unwrap_or_default();
            let stderr = read_err.join().unwrap_or_default();
            return Ok(Some(Captured {
                code: status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            }));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A SIGTERM flag; on non-unix hosts nothing is registered.
pub fn termination_flag() -> (Arc<AtomicBool>, Arc<AtomicI32>) {
    let flag = Arc::new(AtomicBool::new(false));
    let signal = Arc::new(AtomicI32::new(0));
    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, flag.clone());
        signal.store(signal_hook::consts::SIGTERM, Ordering::SeqCst);
    }
    (flag, signal)
}

#[derive(Default)]
pub struct CompileOutcome {
    pub ok: Vec<String>,
    pub failed: Vec<String>,
    pub runs: BTreeMap<String, Value>,
    pub suspect_stale: Option<Value>,
    pub interrupted: Option<i32>,
    pub log: Vec<Line>,
}

/// A stderr line: a timed phase line, or text printed as is.
#[derive(Clone, Debug)]
pub enum Line {
    Progress(f64, String),
    Raw(String),
}

/// Shard-mode `cargo check` of every touched shard, supervised by the caller.
/// `checkpoint` writes the durable partial receipt after each shard (#1718).
pub fn compile_shards(
    loaded: &Loaded,
    ctx: &Ctx,
    shards: &[String],
    packages_of_shard: &BTreeMap<String, Vec<String>>,
    interrupt: &AtomicBool,
    signal: i32,
    clock: Instant,
    checkpoint: &mut dyn FnMut(&str, &CompileOutcome),
) -> CompileOutcome {
    let mut outcome = CompileOutcome::default();
    let mut signatures: BTreeMap<String, String> = BTreeMap::new();
    clean_workspace_packages(loaded, ctx, &mut outcome.log);
    checkpoint("mapped", &outcome);
    for (number, shard) in shards.iter().enumerate() {
        let packages = packages_of_shard
            .get(shard)
            .cloned()
            .unwrap_or_else(|| vec!["rsid".into()]);
        let mut command = cargo_prefix(ctx);
        command.push("check".into());
        for package in &packages {
            command.extend(["-p".into(), package.clone()]);
        }
        command.extend([
            "--lib".into(),
            "--tests".into(),
            "--no-default-features".into(),
            "--features".into(),
            format!("test-shard-{shard}"),
        ]);
        outcome.log.push(Line::Progress(
            clock.elapsed().as_secs_f64(),
            format!(
                "compiling shard {shard} ({}/{}); a held build slot queues first",
                number + 1,
                shards.len()
            ),
        ));
        let started = Instant::now();
        let result = run_cancellable(&command, &loaded.repo.root, &[], interrupt);
        let Ok(Some(result)) = result else {
            outcome.interrupted = Some(signal);
            checkpoint("interrupted", &outcome);
            return outcome;
        };
        let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let waited = slot_wait_seconds(&result.stderr);
        let build = ((seconds - waited as f64).max(0.0) * 10.0).round() / 10.0;
        outcome.runs.insert(
            shard.clone(),
            json!({"verdict": if result.code == 0 { "ok" } else { "fail" }, "seconds": seconds,
                   "slot_wait_sec": waited, "build_sec": build}),
        );
        outcome.log.push(Line::Progress(
            clock.elapsed().as_secs_f64(),
            format!(
                "shard {shard} {} in {seconds:.1}s (build slot wait {waited}s)",
                if result.code == 0 { "ok" } else { "FAILED" }
            ),
        ));
        if result.code == 0 {
            outcome.ok.push(shard.clone());
        } else {
            outcome.failed.push(shard.clone());
            signatures.insert(shard.clone(), error_signature(&result.stderr));
            outcome.log.push(Line::Raw(format!(
                "shard {shard} compile failed:\ncommand: {}\nexit code: {}\n{}",
                command.join(" "),
                result.code,
                failure_excerpt(&result.stderr)
            )));
        }
        checkpoint("compile", &outcome);
    }
    let distinct: std::collections::BTreeSet<_> = signatures.values().collect();
    if outcome.failed.len() >= 2 && outcome.ok.is_empty() && distinct.len() == 1 {
        let first = distinct.into_iter().next().cloned().unwrap_or_default();
        if !first.is_empty() {
            outcome.suspect_stale = Some(
                json!({"hint": STALE_HINT, "error": first.chars().take(500).collect::<String>()}),
            );
            outcome.log.push(Line::Raw(format!(
                "check-touched-shards: shard_compile_suspect_stale: {STALE_HINT}"
            )));
        }
    }
    outcome
}

/// Opt-in (CHECK_TOUCHED_SHARDS_CLEAN=1): drop cached artifacts of every
/// workspace package. Never cleans a target directory outside the repo.
fn clean_workspace_packages(loaded: &Loaded, ctx: &Ctx, log: &mut Vec<Line>) {
    if ctx
        .var("CHECK_TOUCHED_SHARDS_CLEAN")
        .is_none_or(str::is_empty)
    {
        return;
    }
    let repo = &loaded.repo.root;
    let target = match ctx.var("CARGO_TARGET_DIR") {
        Some(dir) => Some(std::path::PathBuf::from(dir)),
        None => Command::new("cargo")
            .args([
                "metadata",
                "--no-deps",
                "--offline",
                "--format-version",
                "1",
            ])
            .current_dir(repo)
            .output()
            .ok()
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
            .and_then(|v| v["target_directory"].as_str().map(std::path::PathBuf::from)),
    };
    let inside = target
        .as_ref()
        .and_then(|t| t.canonicalize().ok())
        .is_some_and(|t| t.starts_with(repo));
    if !inside {
        log.push(Line::Raw(format!(
            "check-touched-shards: not cleaning {}: the target directory is not under {}",
            target.map_or("None".into(), |t| t.display().to_string()),
            repo.display()
        )));
        return;
    }
    let mut crate_dirs: Vec<_> = std::fs::read_dir(repo.join("crates"))
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    // Path-component order: `rsid` sorts before `rsid-store`.
    crate_dirs.sort_by_key(|entry| entry.file_name());
    let names: Vec<String> = crate_dirs
        .into_iter()
        .filter_map(|entry| {
            let text = std::fs::read_to_string(entry.path().join("Cargo.toml")).ok()?;
            let head = text.split("[dependencies]").next().unwrap_or("");
            Re::new(r#"(?m)^\s*name\s*=\s*"([^"]+)""#)
                .search(head)
                .map(|c| crate::re::group(&c, 1).to_owned())
        })
        .collect();
    if names.is_empty() {
        return;
    }
    let mut command = cargo_prefix(ctx);
    command.push("clean".into());
    for name in &names {
        command.extend(["-p".into(), name.clone()]);
    }
    let interrupt = AtomicBool::new(false);
    if let Ok(Some(result)) = run_cancellable(&command, repo, &[], &interrupt) {
        if result.code != 0 {
            let tail: String = result
                .stderr
                .chars()
                .rev()
                .take(500)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            log.push(Line::Raw(format!("check-touched-shards: cargo clean of workspace packages failed (continuing):\n{tail}")));
        }
    }
}

/// A private TMPDIR on tmpfs, the filesystem the merge-queue lander gates on (#1182).
pub fn gate_tmpdir(ctx: &Ctx, log: &mut Vec<String>) -> Option<std::path::PathBuf> {
    let parent = ctx.var("CHECK_TOUCHED_SHARDS_TMPDIR").unwrap_or("/dev/shm");
    let usable = Path::new(parent).is_dir()
        && std::fs::metadata(parent).is_ok_and(|m| !m.permissions().readonly());
    let dir = if usable {
        tempfile::Builder::new()
            .prefix("check-touched-shards-")
            .tempdir_in(parent)
    } else {
        log.push(format!(
            "check-touched-shards: {parent} unusable; filters run with the host TMPDIR"
        ));
        tempfile::Builder::new()
            .prefix("check-touched-shards-")
            .tempdir()
    };
    dir.ok().map(|d| d.keep())
}

/// Run the suggested landing filters (plus any `--filter`) under a tmpfs TMPDIR,
/// as the merge-queue lander does (#1182).
pub fn run_filters(
    loaded: &Loaded,
    ctx: &Ctx,
    filters: &[String],
    packages_of_shard: &BTreeMap<String, Vec<String>>,
    log: &mut Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let (mut ok, mut failed) = (Vec::new(), Vec::new());
    let Some(tmpdir) = gate_tmpdir(ctx, log) else {
        return (ok, failed);
    };
    let shard_filter = Re::new(r"^rsid=shard:([a-z]+-\d+):(.+)$");
    let never = AtomicBool::new(false);
    let mut seen = std::collections::BTreeSet::new();
    for flt in filters.iter().filter(|f| seen.insert((*f).clone())) {
        let Some(m) = shard_filter.search(flt) else {
            continue;
        };
        let (shard, expression) = (
            crate::re::group(&m, 1).to_owned(),
            crate::re::group(&m, 2).to_owned(),
        );
        let packages = packages_of_shard
            .get(&shard)
            .cloned()
            .unwrap_or_else(|| vec!["rsid".into()]);
        let mut command = cargo_prefix(ctx);
        command.extend(["nextest".into(), "run".into()]);
        for package in &packages {
            command.extend(["-p".into(), package.clone()]);
        }
        command.extend([
            "--lib".into(),
            "--no-default-features".into(),
            "--features".into(),
            format!("test-shard-{shard}"),
            "-E".into(),
            expression,
        ]);
        let env = [("TMPDIR".to_owned(), tmpdir.to_string_lossy().into_owned())];
        match run_cancellable(&command, &loaded.repo.root, &env, &never) {
            Ok(Some(result)) if result.code == 0 => ok.push(flt.clone()),
            Ok(Some(result)) => {
                failed.push(flt.clone());
                log.push(format!(
                    "filter {flt} failed with TMPDIR={}:\n{}",
                    tmpdir.display(),
                    tail(&format!("{}{}", result.stdout, result.stderr), 3000)
                ));
            }
            _ => failed.push(flt.clone()),
        }
    }
    let _ = std::fs::remove_dir_all(&tmpdir);
    (ok, failed)
}

/// Lander arguments from an installed `rsi-test-impact`, when present (a missing tool is `None`).
pub fn impact_filters(loaded: &Loaded, ctx: &Ctx) -> Option<Vec<String>> {
    let exe = ctx
        .var("RSI_TEST_IMPACT")
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
        .or_else(|| which("rsi-test-impact"))?;
    let output = Command::new(exe)
        .args([
            "--repo",
            &loaded.repo.root.to_string_lossy(),
            "--base",
            &loaded.ctx.merge_base,
            "--head",
            &loaded.ctx.head,
        ])
        .current_dir(&loaded.repo.root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut filters = Vec::new();
    let mut in_section = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.starts_with("Lander arguments:") {
            in_section = true;
        } else if in_section && line.starts_with("  --test-filter") {
            filters.push(
                line.trim()
                    .split_once(char::is_whitespace)?
                    .1
                    .trim_start()
                    .to_owned(),
            );
        } else if in_section && !line.starts_with("  ") {
            break;
        }
    }
    Some(filters)
}

fn which(name: &str) -> Option<String> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    })
}

/// `json.dumps(value, sort_keys=True)`: keys sorted, `", "` and `": "` separators,
/// ASCII-escaped. The slow-tier decision is printed this way.
pub fn dumps_sorted(value: &Value) -> String {
    fn go(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&python_json(&Value::String((*key).clone()), false));
                    out.push_str(": ");
                    go(&map[*key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    go(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&python_json(other, false)),
        }
    }
    let mut out = String::new();
    go(value, &mut out);
    out
}
