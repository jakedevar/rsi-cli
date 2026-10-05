//! #1097: spill large tool outputs on write, never rewrite history.
//!
//! A tool output over a threshold (default 8 KB or 200 lines) is stored in full
//! under `~/.rsi/spill/<session8>/<seq>.out` (plus a `<seq>.json` sidecar) and
//! the transcript gets a compact deterministic stub instead: exit status, size,
//! the decision-relevant lines (errors, FAILED tests, summary lines), a short
//! head and tail, and the handle. [`show`] returns exact detail on demand
//! (`--grep`, `--range`). Nothing earlier in a conversation is edited, so the
//! prompt-cache prefix stays intact. No model is called: stub generation is a
//! pure function of the output.
//!
//! Consumers: the Claude `PostToolUse`/`PreToolUse` hook
//! ([`crate::boundary_mail_hook`]) and the `rsi-rpc spill` wrapper
//! (`scripts/rsi-spill`) that Codex and other CLI workers run builds and tests
//! through.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;
use serde_json::{Value, json};

/// Default byte threshold above which an output is spilled.
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024;
/// Default line threshold above which an output is spilled.
pub const DEFAULT_MAX_LINES: usize = 200;
/// Hard cap for one stub (the principle says "under 2 KB").
pub const STUB_BUDGET: usize = 1900;
/// Most bytes of one output kept on disk; the rest is dropped with a marker.
pub const STORE_CAP_BYTES: usize = 64 * 1024 * 1024;
/// Byte budget for the decision-relevant lines inside a stub.
const KEY_BUDGET: usize = 900;
/// Longest single line inside a stub.
const KEY_LINE_CHARS: usize = 140;
const HEAD_LINE_CHARS: usize = 110;
const HEAD_LINES: usize = 15;
const TAIL_LINES: usize = 25;

pub const ENV_DIR: &str = "RSI_SPILL_DIR";
pub const ENV_BYTES: &str = "RSI_SPILL_BYTES";
pub const ENV_LINES: &str = "RSI_SPILL_LINES";
pub const ENV_DISABLE: &str = "RSI_SPILL_DISABLE";
/// Set to `0` to stop the hook rewriting heavy Bash commands through the wrapper.
pub const ENV_PRE: &str = "RSI_SPILL_PRE";
/// Set to `1` to also spill `Read` results (off by default: a Read is a
/// deliberate request for exact content).
pub const ENV_READ: &str = "RSI_SPILL_READ";

/// File name of the kill switch inside the store root.
pub const KILL_SWITCH_FILE: &str = "DISABLED";

/// True when `<root>/DISABLED` exists: an operator switch that, unlike the
/// environment, reaches sessions that are already running (the hook re-reads it
/// on every tool call).
#[must_use]
pub fn kill_switch_present(root: &Path) -> bool {
    root.join(KILL_SWITCH_FILE).exists()
}

/// Thresholds and the store root for one process.
#[derive(Debug, Clone)]
pub struct SpillConfig {
    pub root: PathBuf,
    pub max_bytes: usize,
    pub max_lines: usize,
    pub disabled: bool,
}

impl SpillConfig {
    /// Build from the environment (`RSI_SPILL_*`), defaults otherwise.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(&|name| std::env::var(name).ok())
    }

    /// [`Self::from_env`] with an injectable variable lookup (for tests).
    #[must_use]
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let number = |name: &str, default: usize| {
            get(name)
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(default)
        };
        let root = get(ENV_DIR)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::identity::data_path("spill", "spill"));
        let disabled = get(ENV_DISABLE).is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"))
            || kill_switch_present(&root);
        Self {
            root,
            max_bytes: number(ENV_BYTES, DEFAULT_MAX_BYTES),
            max_lines: number(ENV_LINES, DEFAULT_MAX_LINES),
            disabled,
        }
    }

    /// True when `text` is large enough to spill.
    #[must_use]
    pub fn exceeds(&self, text: &str) -> bool {
        text.len() > self.max_bytes || text.lines().count() > self.max_lines
    }
}

/// Sanitised short session key used in handles and directory names.
#[must_use]
pub fn session_key(raw: &str) -> String {
    let key: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(8)
        .collect();
    if key.is_empty() {
        "local".to_string()
    } else {
        key
    }
}

/// The session key for this process: `$RSI_SESSION_ID`, else `fallback`.
#[must_use]
pub fn current_session_key(fallback: Option<&str>) -> String {
    let from_env = std::env::var("RSI_SESSION_ID")
        .ok()
        .filter(|v| !v.is_empty());
    session_key(from_env.as_deref().or(fallback).unwrap_or("local"))
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();
        for next in chars.by_ref() {
            if ('@'..='~').contains(&next) {
                break;
            }
        }
    }
    out
}

fn truncate_chars(line: &str, max: usize) -> String {
    let line = line.trim_end();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut cut: String = line.chars().take(max).collect();
    cut.push('…');
    cut
}

/// Priority of a decision-relevant line (lower is more important), or `None`.
fn key_priority(line: &str) -> Option<u8> {
    let t = line.trim_start();
    if t.starts_with("test result:") {
        return Some(0);
    }
    if (t.starts_with("test ") && t.trim_end().ends_with("FAILED"))
        || t.starts_with("FAILED ")
        || t.starts_with("error: could not compile")
        || t.starts_with("error: test failed")
        || t.starts_with("failures:")
        || t.starts_with("---- ") && t.trim_end().ends_with("stdout ----")
    {
        return Some(1);
    }
    if t.starts_with("error[")
        || t.starts_with("error:")
        || t.starts_with("Error:")
        || t.starts_with("fatal:")
        || t.contains("panicked at")
    {
        return Some(2);
    }
    if t.starts_with("warning:") && t.contains("generated") {
        return Some(3);
    }
    None
}

/// Deterministic stub (no model call), at most [`STUB_BUDGET`] bytes.
#[must_use]
pub fn build_stub(handle: &str, tool: &str, exit: Option<i32>, text: &str) -> String {
    let lines_total = text.lines().count();
    let exit_label = exit.map_or_else(|| "?".to_string(), |n| n.to_string());
    let header = format!(
        "[rsi-spill {handle}] {tool} exit={exit_label} {}B {lines_total} lines",
        text.len()
    );
    let footer = format!("full: rsi-spill show {handle} [--grep P] [--range a:b]");
    let clean = strip_ansi(text);
    let lines: Vec<&str> = clean.lines().collect();

    // Decision-relevant lines, picked by priority then original order.
    let mut candidates: Vec<(u8, usize, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(priority) = key_priority(line) else {
            continue;
        };
        let shown = truncate_chars(line, KEY_LINE_CHARS);
        if seen.insert(shown.clone()) {
            candidates.push((priority, index, shown));
        }
    }
    let total_keys = candidates.len();
    candidates.sort_by_key(|(priority, index, _)| (*priority, *index));
    let mut picked: Vec<(usize, String)> = Vec::new();
    let mut used = 0usize;
    for (_, index, shown) in candidates {
        if used + shown.len() + 1 > KEY_BUDGET {
            continue;
        }
        used += shown.len() + 1;
        picked.push((index, shown));
    }
    picked.sort_by_key(|(index, _)| *index);
    let more_keys = total_keys - picked.len();

    let mut stub = header;
    if !picked.is_empty() {
        stub.push_str("\n-- key lines --");
        for (_, shown) in &picked {
            stub.push('\n');
            stub.push_str(shown);
        }
        if more_keys > 0 {
            stub.push_str(&format!(
                "\n(+{more_keys} more: rsi-spill show {handle} --grep 'FAILED|error')"
            ));
        }
    }

    // Head and tail share what is left of the budget.
    let fixed = stub.len() + footer.len() + 80;
    let remaining = STUB_BUDGET.saturating_sub(fixed);
    let head_budget = remaining * 2 / 5;
    let tail_budget = remaining - head_budget;
    let mut head: Vec<String> = Vec::new();
    let mut spent = 0usize;
    for line in lines.iter().take(HEAD_LINES) {
        let shown = truncate_chars(line, HEAD_LINE_CHARS);
        if spent + shown.len() + 1 > head_budget {
            break;
        }
        spent += shown.len() + 1;
        head.push(shown);
    }
    let mut tail: Vec<String> = Vec::new();
    let mut spent = 0usize;
    for line in lines.iter().skip(head.len()).rev().take(TAIL_LINES) {
        let shown = truncate_chars(line, HEAD_LINE_CHARS);
        if spent + shown.len() + 1 > tail_budget {
            break;
        }
        spent += shown.len() + 1;
        tail.push(shown);
    }
    tail.reverse();
    let omitted = lines.len().saturating_sub(head.len() + tail.len());

    if !head.is_empty() {
        stub.push_str("\n-- head --\n");
        stub.push_str(&head.join("\n"));
    }
    if omitted > 0 {
        stub.push_str(&format!("\n... {omitted} lines omitted ..."));
    }
    if !tail.is_empty() {
        stub.push_str("\n-- tail --\n");
        stub.push_str(&tail.join("\n"));
    }
    stub.push('\n');
    stub.push_str(&footer);
    stub
}

/// What to record about a spilled output.
#[derive(Debug, Clone, Default)]
pub struct SpillMeta<'a> {
    pub tool: &'a str,
    pub command: Option<&'a str>,
    pub exit: Option<i32>,
}

fn next_seq(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".out")?.parse::<u64>().ok()
                })
                .max()
                .unwrap_or(0)
                + 1
        })
        .unwrap_or(1)
}

/// Create the next `<seq>.out` under `<root>/<key>/` and return its handle,
/// path and open file.
fn create_slot(root: &Path, key: &str) -> std::io::Result<(String, PathBuf, std::fs::File)> {
    let dir = root.join(key);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let mut seq = next_seq(&dir);
    loop {
        let path = dir.join(format!("{seq}.out"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((format!("{key}/{seq}"), path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => seq += 1,
            Err(e) => return Err(e),
        }
    }
}

/// (Re)write the `<seq>.json` sidecar next to `out_path`.
fn write_sidecar(out_path: &Path, meta: &SpillMeta<'_>, bytes: usize, lines: usize, running: bool) {
    let sidecar = json!({
        "tool": meta.tool,
        "command": meta.command,
        "exit": meta.exit,
        "bytes": bytes,
        "lines": lines,
        "running": running,
        "created_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    });
    let sidecar_path = out_path.with_extension("json");
    if std::fs::write(&sidecar_path, sidecar.to_string()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sidecar_path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn truncation_marker() -> String {
    format!("\n[rsi-spill: output truncated at {STORE_CAP_BYTES} bytes]\n")
}

/// Store `data` under a fresh handle `<key>/<seq>` and return the handle.
pub fn store(root: &Path, key: &str, meta: &SpillMeta<'_>, data: &[u8]) -> std::io::Result<String> {
    let (handle, path, mut file) = create_slot(root, key)?;
    file.write_all(&data[..data.len().min(STORE_CAP_BYTES)])?;
    if data.len() > STORE_CAP_BYTES {
        file.write_all(truncation_marker().as_bytes())?;
    }
    let lines = String::from_utf8_lossy(data).lines().count();
    write_sidecar(&path, meta, data.len(), lines, false);
    Ok(handle)
}

/// Store `text` and return `(handle, stub)` when it is over the thresholds and
/// the stub actually saves space; `None` means "leave the output alone".
#[must_use]
pub fn spill_text(
    cfg: &SpillConfig,
    key: &str,
    meta: &SpillMeta<'_>,
    text: &str,
) -> Option<(String, String)> {
    if cfg.disabled || !cfg.exceeds(text) {
        return None;
    }
    let handle = store(&cfg.root, key, meta, text.as_bytes()).ok()?;
    let stub = build_stub(&handle, meta.tool, meta.exit, text);
    if stub.len() * 4 > text.len() * 3 {
        // No real saving: keep the output inline; the stored copy is harmless.
        return None;
    }
    Some((handle, stub))
}

fn valid_component(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse `a:b`, `a:`, `:b` or `a` (1-based, inclusive) into line bounds.
pub fn parse_range(spec: &str) -> Result<(usize, usize), String> {
    let bad = || format!("bad --range '{spec}' (expected a:b, a:, :b or a)");
    let number = |s: &str| s.trim().parse::<usize>().map_err(|_| bad());
    let (start, end) = match spec.split_once(':') {
        None => {
            let n = number(spec)?;
            (n, n)
        }
        Some((a, b)) => (
            if a.trim().is_empty() { 1 } else { number(a)? },
            if b.trim().is_empty() {
                usize::MAX
            } else {
                number(b)?
            },
        ),
    };
    if start == 0 || end < start {
        return Err(bad());
    }
    Ok((start, end))
}

/// Read a spilled output: whole, a line range, and/or the lines matching a
/// regex (invalid regexes fall back to a literal match). With `grep`, lines
/// are prefixed `N:` so a follow-up `--range` can fetch context.
pub fn show(
    root: &Path,
    handle: &str,
    grep: Option<&str>,
    range: Option<(usize, usize)>,
) -> Result<String, String> {
    let (key, seq) = handle
        .split_once('/')
        .ok_or_else(|| format!("bad handle '{handle}' (expected <session>/<n>)"))?;
    if !valid_component(key) || !valid_component(seq) {
        return Err(format!("bad handle '{handle}'"));
    }
    let path = root.join(key).join(format!("{seq}.out"));
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let pattern = grep
        .map(|p| Regex::new(p).or_else(|_| Regex::new(&regex::escape(p))))
        .transpose()
        .map_err(|e| e.to_string())?;
    let (start, end) = range.unwrap_or((1, usize::MAX));
    let mut out = String::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if number < start {
            continue;
        }
        if number > end {
            break;
        }
        match &pattern {
            Some(re) if !re.is_match(line) => continue,
            Some(_) => out.push_str(&format!("{number}:{line}\n")),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    Ok(out)
}

/// Run a command, merging stderr into stdout; spill when large. `shell` runs
/// the single argument through `bash -c`. Returns the child's exit code (128+N
/// for a signal). Output small enough is passed through byte for byte.
pub fn run_wrapped(
    cfg: &SpillConfig,
    key: &str,
    argv: &[String],
    shell: bool,
    out: &mut impl Write,
) -> i32 {
    let mut command = if shell {
        let mut c = Command::new("bash");
        c.arg("-c").arg(format!("exec 2>&1\n{}", argv.join(" ")));
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg("exec \"$@\" 2>&1").arg("rsi-spill");
        c.args(argv);
        c
    };
    let label = argv.join(" ");
    let mut meta = SpillMeta {
        tool: "Bash",
        command: Some(&label),
        exit: None,
    };
    // The slot (and the handle line) exist before the command starts, so a run
    // the caller kills still leaves a handle for `rsi-spill show` (#1097).
    let slot = if cfg.disabled {
        None
    } else {
        create_slot(&cfg.root, key).ok()
    };
    command.stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = writeln!(out, "rsi-spill: cannot run command: {e}");
            return 127;
        }
    };
    let (mut file, path, handle) = match slot {
        Some((handle, path, file)) => (Some(file), Some(path), Some(handle)),
        None => (None, None, None),
    };
    if let (Some(handle), Some(path)) = (&handle, &path) {
        write_sidecar(path, &meta, 0, 0, true);
        let _ = writeln!(
            out,
            "[rsi-spill {handle}] running: {}",
            header_command(&label)
        );
        let _ = out.flush();
    }
    let mut data = Vec::new();
    let mut total = 0usize;
    if let Some(mut stdout) = child.stdout.take() {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            total += n;
            let room = STORE_CAP_BYTES.saturating_sub(data.len());
            let keep = n.min(room);
            if keep > 0 {
                data.extend_from_slice(&buf[..keep]);
                // Unbuffered append: a killed wrapper keeps everything so far.
                if let Some(f) = file.as_mut() {
                    let _ = f.write_all(&buf[..keep]);
                }
            }
            // Past the cap keep draining so the child never blocks on a pipe.
        }
    }
    let code = match child.wait() {
        Ok(status) => exit_code(status),
        Err(_) => 1,
    };
    meta.exit = Some(code);
    let text = String::from_utf8_lossy(&data);
    if total > STORE_CAP_BYTES
        && let Some(f) = file.as_mut()
    {
        let _ = f.write_all(truncation_marker().as_bytes());
    }
    if let Some(path) = &path {
        write_sidecar(path, &meta, total, text.lines().count(), false);
    }
    let stub = handle
        .as_deref()
        .filter(|_| cfg.exceeds(&text))
        .map(|h| build_stub(h, meta.tool, meta.exit, &text))
        .filter(|stub| stub.len() * 4 <= text.len() * 3);
    match stub {
        Some(stub) => {
            let _ = writeln!(out, "{stub}");
        }
        None => {
            let _ = out.write_all(&data);
        }
    }
    code
}

/// The command as shown on the `running:` header line (one line, bounded).
fn header_command(label: &str) -> String {
    let one_line = label.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&one_line, 200)
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

/// `rsi-rpc spill ...` entry point. Returns the process exit code.
pub fn cli_main(args: &[String], out: &mut impl Write, err: &mut impl Write) -> i32 {
    let usage = "usage: rsi-spill -- <cmd> [args...]\n       rsi-spill --shell '<command line>'\n       rsi-spill show <session/n> [--grep PATTERN] [--range a:b]";
    let cfg = SpillConfig::from_env();
    match args.first().map(String::as_str) {
        Some("show") => {
            let mut handle = None;
            let mut grep = None;
            let mut range = None;
            let mut iter = args[1..].iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--grep" => grep = iter.next().cloned(),
                    "--range" => match iter.next().map(|s| parse_range(s)) {
                        Some(Ok(r)) => range = Some(r),
                        Some(Err(e)) => {
                            let _ = writeln!(err, "rsi-spill: {e}");
                            return 2;
                        }
                        None => {
                            let _ = writeln!(err, "rsi-spill: --range needs a value");
                            return 2;
                        }
                    },
                    other if handle.is_none() => handle = Some(other.to_string()),
                    other => {
                        let _ = writeln!(err, "rsi-spill: unexpected argument '{other}'");
                        return 2;
                    }
                }
            }
            let Some(handle) = handle else {
                let _ = writeln!(err, "{usage}");
                return 2;
            };
            match show(&cfg.root, &handle, grep.as_deref(), range) {
                Ok(text) => {
                    let _ = out.write_all(text.as_bytes());
                    0
                }
                Err(e) => {
                    let _ = writeln!(err, "rsi-spill: {e}");
                    1
                }
            }
        }
        Some("--shell") if args.len() >= 2 => {
            let key = current_session_key(None);
            run_wrapped(&cfg, &key, &args[1..], true, out)
        }
        Some("--") if args.len() >= 2 => {
            let key = current_session_key(None);
            run_wrapped(&cfg, &key, &args[1..], false, out)
        }
        _ => {
            let _ = writeln!(err, "{usage}");
            2
        }
    }
}

/// Single-quote `text` for a POSIX shell.
#[must_use]
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The text of a shell command that the shell runs as code: quoted strings,
/// here-strings, heredoc bodies and comments are blanked, so a command that
/// only mentions `cargo test` inside a prompt or a file it writes is not
/// routed (#1105). From an unterminated quote on, the raw text is kept, so an
/// unparsable command is still matched as before.
fn shell_code_text(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    // Heredoc delimiters opened on the current line: (delimiter, strip tabs).
    let mut pending: Vec<(String, bool)> = Vec::new();
    let mut word_start = true;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                out.push(' ');
                i += 2;
                word_start = false;
            }
            '\'' | '"' => {
                let Some(end) = closing_quote(&chars, i) else {
                    out.extend(&chars[i..]);
                    break;
                };
                out.push(' ');
                i = end + 1;
                word_start = false;
            }
            '#' if word_start => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                out.push(' ');
                if chars.get(i + 2) == Some(&'<') {
                    // A here-string's word is data.
                    i = skip_blanks(&chars, i + 3);
                    let Some((_, next)) = shell_word(&chars, i) else {
                        out.extend(&chars[i..]);
                        break;
                    };
                    i = next;
                } else {
                    let strip_tabs = chars.get(i + 2) == Some(&'-');
                    i = skip_blanks(&chars, i + if strip_tabs { 3 } else { 2 });
                    let Some((delimiter, next)) = shell_word(&chars, i) else {
                        out.extend(&chars[i..]);
                        break;
                    };
                    pending.push((delimiter, strip_tabs));
                    i = next;
                }
                word_start = false;
            }
            '\n' => {
                out.push('\n');
                i += 1;
                for (delimiter, strip_tabs) in pending.drain(..) {
                    while i < chars.len() {
                        let end = chars[i..]
                            .iter()
                            .position(|&ch| ch == '\n')
                            .map_or(chars.len(), |offset| i + offset);
                        let line: String = chars[i..end].iter().collect();
                        i = (end + 1).min(chars.len());
                        let line = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            line.as_str()
                        };
                        if line == delimiter {
                            break;
                        }
                    }
                }
                word_start = true;
            }
            _ => {
                out.push(c);
                i += 1;
                word_start = c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')');
            }
        }
    }
    out
}

/// Index of the quote closing the one at `open` (`\` escapes inside `"`).
fn closing_quote(chars: &[char], open: usize) -> Option<usize> {
    let quote = chars[open];
    let mut i = open + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' if quote == '"' => i += 2,
            ch if ch == quote => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn skip_blanks(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && matches!(chars[i], ' ' | '\t') {
        i += 1;
    }
    i
}

/// One shell word starting at `i` with its quotes removed, and the index after
/// it; `None` for an empty word or an unterminated quote.
fn shell_word(chars: &[char], mut i: usize) -> Option<(String, usize)> {
    let mut word = String::new();
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' => {
                let end = closing_quote(chars, i)?;
                word.extend(&chars[i + 1..end]);
                i = end + 1;
            }
            ch if ch.is_whitespace() || matches!(ch, ';' | '&' | '|' | '<' | '>' | '(' | ')') => {
                break;
            }
            ch => {
                word.push(ch);
                i += 1;
            }
        }
    }
    (!word.is_empty()).then_some((word, i))
}

/// True for commands that routinely print megabytes (builds, tests, logs).
/// Only the text the shell runs as code is matched ([`shell_code_text`]).
#[must_use]
pub fn is_heavy_command(command: &str) -> bool {
    use std::sync::OnceLock;
    static HEAVY: OnceLock<Regex> = OnceLock::new();
    let re = HEAVY.get_or_init(|| {
        Regex::new(
            r"(?x)
            (?:^|[\s;&|(])cargo\s+(?:\+\S+\s+)?(?:build|test|check|clippy|nextest|bench|doc)\b
            | run-rsid-test-shards
            | (?:^|[\s;&|(])(?:npm|pnpm|yarn)\s+(?:test|run\s+(?:build|test))\b
            | (?:^|[\s;&|(])(?:pytest|go\s+test|journalctl|make)\b
            ",
        )
        .expect("heavy command regex")
    });
    re.is_match(&shell_code_text(command))
}

/// True when the command already goes through (or reads from) the spill store.
#[must_use]
pub fn is_spill_command(command: &str) -> bool {
    command.contains("rsi-spill")
        || command.contains(" spill show")
        || command.contains(" spill --")
}

/// `PreToolUse` rewrite: route a heavy foreground Bash command through the
/// wrapper (`wrapper` is the raw program path; it is shell-quoted here).
/// Returns the new `tool_input`, or `None` to leave it alone.
#[must_use]
pub fn rewrite_bash_input(input: &Value, wrapper: &str) -> Option<Value> {
    let tool_input = input.get("tool_input")?;
    let command = tool_input.get("command")?.as_str()?;
    let background = tool_input
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if background || is_spill_command(command) || !is_heavy_command(command) {
        return None;
    }
    let mut updated = tool_input.clone();
    updated["command"] = Value::String(format!(
        "{} spill --shell {}",
        shell_quote(wrapper),
        shell_quote(command)
    ));
    Some(updated)
}

/// `PostToolUse` spill: given the hook input, return the replacement
/// `tool_response` (same shape as the original, text swapped for a stub) when
/// the output is large, else `None`.
#[must_use]
pub fn spill_tool_response(input: &Value, cfg: &SpillConfig) -> Option<Value> {
    if cfg.disabled {
        return None;
    }
    let tool = input.get("tool_name")?.as_str()?;
    let response = input.get("tool_response")?;
    let key = current_session_key(input.get("session_id").and_then(Value::as_str));
    let command = input
        .get("tool_input")
        .and_then(|i| i.get("command"))
        .and_then(Value::as_str);
    if tool == "Bash" && command.is_some_and(is_spill_command) {
        return None;
    }
    let meta = SpillMeta {
        tool,
        command,
        exit: Some(0),
    };
    let mut updated = response.clone();
    match tool {
        "Bash" => {
            let flag = |name: &str| response.get(name).and_then(Value::as_bool).unwrap_or(false);
            if flag("isImage")
                || response.get("backgroundTaskId").is_some()
                || response.get("persistedOutputPath").is_some()
            {
                return None;
            }
            let stdout = response.get("stdout")?.as_str()?;
            let stderr = response.get("stderr").and_then(Value::as_str).unwrap_or("");
            let text = if stderr.is_empty() {
                stdout.to_string()
            } else {
                format!("{stdout}\n[stderr]\n{stderr}")
            };
            let (_, stub) = spill_text(cfg, &key, &meta, &text)?;
            updated["stdout"] = Value::String(stub);
            updated["stderr"] = Value::String(String::new());
        }
        "Grep" => {
            let content_mode = response.get("mode").and_then(Value::as_str) == Some("content");
            if content_mode {
                let text = response.get("content")?.as_str()?;
                let (_, stub) = spill_text(cfg, &key, &meta, text)?;
                updated["content"] = Value::String(stub);
                updated["filenames"] = json!([]);
            } else {
                let names = response.get("filenames")?.as_array()?;
                let text = names
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n");
                let (_, stub) = spill_text(cfg, &key, &meta, &text)?;
                updated["filenames"] = json!([stub]);
            }
        }
        "Read" => {
            let enabled = std::env::var(ENV_READ).is_ok_and(|v| v.trim() == "1");
            if !enabled {
                return None;
            }
            let content = response.get("file")?.get("content")?.as_str()?;
            let (_, stub) = spill_text(cfg, &key, &meta, content)?;
            updated["file"]["content"] = Value::String(stub);
        }
        _ => return None,
    }
    Some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(root: &Path) -> SpillConfig {
        SpillConfig {
            root: root.to_path_buf(),
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            disabled: false,
        }
    }

    fn cargo_test_output(passing: usize) -> String {
        let mut out = String::from("   Compiling rsid v0.1.0\nrunning 5000 tests\n");
        for i in 0..passing {
            out.push_str(&format!("test store::tests::case_{i} ... ok\n"));
        }
        out.push_str("test store::tests::migrate_v7 ... FAILED\n");
        out.push_str("test rpc::tests::roundtrip ... FAILED\n");
        out.push_str("\nfailures:\n\n---- store::tests::migrate_v7 stdout ----\n");
        out.push_str(
            "thread 'store::tests::migrate_v7' panicked at src/store.rs:42:9:\nassertion failed\n",
        );
        out.push_str("\nfailures:\n    store::tests::migrate_v7\n    rpc::tests::roundtrip\n\n");
        out.push_str("test result: FAILED. 4998 passed; 2 failed; 0 ignored; 0 measured\n");
        out.push_str("error: test failed, to rerun pass `-p rsid --lib`\n");
        out
    }

    #[test]
    fn cargo_test_stub_names_failures_handle_and_stays_small() {
        let text = cargo_test_output(50_000);
        assert!(text.len() > 1_500_000);
        let stub = build_stub("abcd1234/7", "Bash", Some(101), &text);
        assert!(stub.len() < 2000, "stub is {} bytes", stub.len());
        assert!(
            stub.starts_with("[rsi-spill abcd1234/7] Bash exit=101 "),
            "{stub}"
        );
        assert!(stub.contains("store::tests::migrate_v7 ... FAILED"));
        assert!(stub.contains("rpc::tests::roundtrip ... FAILED"));
        assert!(stub.contains("test result: FAILED. 4998 passed; 2 failed"));
        assert!(stub.contains("full: rsi-spill show abcd1234/7"));
        assert!(stub.contains("-- head --") && stub.contains("-- tail --"));
        assert_eq!(stub, build_stub("abcd1234/7", "Bash", Some(101), &text));
    }

    #[test]
    fn cargo_build_error_stub_keeps_errors_and_strips_ansi() {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(&format!("   Compiling crate{i} v1.0.{i}\n"));
        }
        text.push_str("\u{1b}[1m\u{1b}[31merror[E0308]\u{1b}[0m: mismatched types\n");
        text.push_str("  --> src/lib.rs:10:5\n");
        text.push_str("warning: `rsid` (lib) generated 12 warnings\n");
        text.push_str("error: could not compile `rsid` (lib) due to 1 previous error\n");
        let stub = build_stub("s/1", "Bash", Some(101), &text);
        assert!(stub.len() < 2000);
        assert!(stub.contains("error[E0308]: mismatched types"), "{stub}");
        assert!(stub.contains("generated 12 warnings"));
        assert!(stub.contains("error: could not compile `rsid`"));
        assert!(!stub.contains('\u{1b}'));
    }

    #[test]
    fn plain_text_stub_has_head_tail_and_omitted_count() {
        let text: String = (0..1000).map(|i| format!("line number {i}\n")).collect();
        let stub = build_stub("s/2", "Grep", Some(0), &text);
        assert!(stub.len() < 2000);
        assert!(!stub.contains("-- key lines --"));
        assert!(stub.contains("line number 0\n"));
        assert!(stub.contains("line number 999"));
        assert!(stub.contains("lines omitted"));
        assert!(!stub.contains("line number 500\n"));
    }

    #[test]
    fn stub_never_exceeds_budget_for_hostile_input() {
        let wide = format!("error: {}\n", "x".repeat(5000)).repeat(400);
        assert!(build_stub("s/3", "Bash", None, &wide).len() < 2000);
        let failed: String = (0..3000)
            .map(|i| format!("test very::long::module::path::test_{i} ... FAILED\n"))
            .collect();
        let stub = build_stub("s/4", "Bash", Some(101), &failed);
        assert!(stub.len() < 2000, "{}", stub.len());
        assert!(stub.contains("more: rsi-spill show s/4 --grep"));
    }

    #[test]
    fn threshold_is_bytes_or_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(tmp.path());
        assert!(!c.exceeds("short\n"));
        assert!(c.exceeds(&"x".repeat(DEFAULT_MAX_BYTES + 1)));
        assert!(c.exceeds(&"y\n".repeat(DEFAULT_MAX_LINES + 1)));
        assert!(!c.exceeds(&"y\n".repeat(DEFAULT_MAX_LINES)));
    }

    #[test]
    fn spill_text_stores_full_output_and_show_returns_exact_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(tmp.path());
        let text = cargo_test_output(3000);
        let meta = SpillMeta {
            tool: "Bash",
            command: Some("cargo test"),
            exit: Some(101),
        };
        let (handle, stub) = spill_text(&c, "sess1234", &meta, &text).expect("spilled");
        assert_eq!(handle, "sess1234/1");
        assert!(stub.len() < 2000);
        let stored = std::fs::read_to_string(tmp.path().join("sess1234/1.out")).unwrap();
        assert_eq!(stored, text);
        let side: Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("sess1234/1.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(side["tool"], "Bash");
        assert_eq!(side["exit"], 101);
        assert_eq!(side["bytes"], text.len());
        let grepped = show(
            tmp.path(),
            &handle,
            Some("migrate_v7 \\.\\.\\. FAILED"),
            None,
        )
        .unwrap();
        assert!(
            grepped.ends_with("test store::tests::migrate_v7 ... FAILED\n"),
            "{grepped}"
        );
        assert!(grepped.contains(':'));
        let ranged = show(tmp.path(), &handle, None, Some((1, 2))).unwrap();
        assert_eq!(ranged, "   Compiling rsid v0.1.0\nrunning 5000 tests\n");
        let (handle2, _) = spill_text(&c, "sess1234", &meta, &text).unwrap();
        assert_eq!(handle2, "sess1234/2");
    }

    #[test]
    fn small_output_is_left_alone_and_disabled_is_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = cfg(tmp.path());
        let meta = SpillMeta {
            tool: "Bash",
            ..Default::default()
        };
        assert!(spill_text(&c, "k", &meta, "tiny\n").is_none());
        c.disabled = true;
        assert!(spill_text(&c, "k", &meta, &"z\n".repeat(5000)).is_none());
    }

    #[test]
    fn show_rejects_path_traversal_and_bad_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(show(tmp.path(), "../etc/1", None, None).is_err());
        assert!(show(tmp.path(), "a/../../b", None, None).is_err());
        assert!(show(tmp.path(), "nohandle", None, None).is_err());
        assert_eq!(parse_range("3:5"), Ok((3, 5)));
        assert_eq!(parse_range("7"), Ok((7, 7)));
        assert_eq!(parse_range(":4"), Ok((1, 4)));
        assert_eq!(parse_range("9:"), Ok((9, usize::MAX)));
        assert!(parse_range("5:2").is_err());
        assert!(parse_range("x").is_err());
    }

    #[test]
    fn bash_hook_replaces_stdout_with_stub_and_keeps_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(tmp.path());
        let big = "noise line\n".repeat(5000);
        let input = json!({
            "session_id": "feedface-0000",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -R /"},
            "tool_response": {"stdout": big, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false},
        });
        let updated = spill_tool_response(&input, &c).expect("spilled");
        let stdout = updated["stdout"].as_str().unwrap();
        assert!(stdout.starts_with("[rsi-spill "), "{stdout}");
        assert!(stdout.len() < 2000);
        assert_eq!(updated["stderr"], "");
        assert_eq!(updated["interrupted"], false);
        assert_eq!(updated["isImage"], false);
        let small = json!({"session_id": "x", "tool_name": "Bash", "tool_input": {"command": "echo hi"},
            "tool_response": {"stdout": "hi\n", "stderr": "", "interrupted": false}});
        assert!(spill_tool_response(&small, &c).is_none());
    }

    #[test]
    fn spill_show_commands_and_background_tasks_are_never_spilled() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(tmp.path());
        let big = "noise line\n".repeat(5000);
        let show_cmd = json!({"session_id": "x", "tool_name": "Bash",
            "tool_input": {"command": "rsi-spill show abcd1234/1 --grep FAILED"},
            "tool_response": {"stdout": big, "stderr": ""}});
        assert!(spill_tool_response(&show_cmd, &c).is_none());
        let background = json!({"session_id": "x", "tool_name": "Bash",
            "tool_input": {"command": "cargo build"},
            "tool_response": {"stdout": big, "stderr": "", "backgroundTaskId": "b1"}});
        assert!(spill_tool_response(&background, &c).is_none());
    }

    #[test]
    fn grep_hook_replaces_content_in_content_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(tmp.path());
        let big = "src/a.rs:10:match here\n".repeat(2000);
        let input = json!({"session_id": "x", "tool_name": "Grep", "tool_input": {"pattern": "match"},
            "tool_response": {"mode": "content", "content": big, "filenames": ["src/a.rs"], "numFiles": 1, "numLines": 2000}});
        let updated = spill_tool_response(&input, &c).expect("spilled");
        assert!(
            updated["content"]
                .as_str()
                .unwrap()
                .starts_with("[rsi-spill ")
        );
        assert_eq!(updated["filenames"], json!([]));
        assert_eq!(updated["numLines"], 2000);
    }

    #[test]
    fn heavy_bash_commands_are_routed_through_the_wrapper() {
        let rewrite = |command: &str| {
            rewrite_bash_input(
                &json!({"tool_input": {"command": command, "description": "d"}}),
                "/bin/rsi-rpc",
            )
        };
        let routed = rewrite("cd crates/rsid && cargo test --lib foo").expect("routed");
        assert_eq!(
            routed["command"],
            "'/bin/rsi-rpc' spill --shell 'cd crates/rsid && cargo test --lib foo'"
        );
        assert_eq!(routed["description"], "d");
        assert!(rewrite("~/.rsi/bin/cargo-slot env -u X cargo test -p rsid").is_some());
        assert!(rewrite("scripts/run-rsid-test-shards.sh").is_some());
        assert!(
            rewrite("echo it's fine; cargo build").unwrap()["command"]
                .as_str()
                .unwrap()
                .contains("'echo it'\\''s fine; cargo build'")
        );
        assert!(rewrite("ls -la").is_none());
        assert!(rewrite("git status").is_none());
        assert!(rewrite("rsi-spill show a/1").is_none());
        assert!(rewrite("cargo fmt --check").is_none());
        let background = rewrite_bash_input(
            &json!({"tool_input": {"command": "cargo build", "run_in_background": true}}),
            "rsi-rpc",
        );
        assert!(background.is_none());
    }

    #[test]
    fn only_commands_the_shell_runs_are_heavy_not_quoted_or_heredoc_text() {
        // #1105: prompts and files that merely mention a heavy command.
        assert!(!is_heavy_command(
            "cat > /tmp/w.md <<'EOF'\nRun cargo test -p rsid --lib foo.\nEOF"
        ));
        assert!(!is_heavy_command(
            "cat <<-EOF > x\n\tmake release-install\n\tEOF\ngit status"
        ));
        assert!(!is_heavy_command("echo \"then cargo build --release\""));
        assert!(!is_heavy_command("printf '%s\\n' 'cargo check' | wc -l"));
        assert!(!is_heavy_command("grep -c x <<< 'pytest -q'"));
        assert!(!is_heavy_command("ls # cargo test later"));
        assert!(!is_heavy_command("echo \"a \\\"cargo test\\\" b\""));
        // Real heavy commands keep routing, including after a heredoc.
        assert!(is_heavy_command(
            "cat > f <<'EOF'\nhello\nEOF\ncargo test -p rsid"
        ));
        assert!(is_heavy_command(
            "cat > a <<A > b <<B\nmake\nA\npytest\nB\nmake -j4"
        ));
        assert!(is_heavy_command("out=$(cargo build 2>&1); echo \"$out\""));
        assert!(is_heavy_command("echo 'x' && cargo nextest run"));
        assert!(is_heavy_command("timeout 590 cargo test -p rsid --lib a#b"));
        // An unterminated quote falls back to the raw text.
        assert!(is_heavy_command("echo it's fine; cargo build"));
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_passes_small_output_and_spills_large_with_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(tmp.path());
        let mut out = Vec::new();
        let code = run_wrapped(
            &c,
            "wrap0001",
            &["echo".into(), "hello".into()],
            false,
            &mut out,
        );
        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "[rsi-spill wrap0001/1] running: echo hello\nhello\n"
        );

        let mut out = Vec::new();
        let code = run_wrapped(
            &c,
            "wrap0001",
            &["seq 1 5000; echo 'test a::b ... FAILED' >&2; exit 7".into()],
            true,
            &mut out,
        );
        assert_eq!(code, 7);
        let printed = String::from_utf8(out).unwrap();
        let (first, stub) = printed.split_once('\n').unwrap();
        assert!(
            first.starts_with("[rsi-spill wrap0001/2] running: seq 1 5000"),
            "{first}"
        );
        assert!(
            stub.starts_with("[rsi-spill wrap0001/2] Bash exit=7 "),
            "{stub}"
        );
        assert!(stub.contains("test a::b ... FAILED"));
        assert!(stub.len() < 2100);
        let full = show(tmp.path(), "wrap0001/2", None, Some((5000, 5000))).unwrap();
        assert_eq!(full, "5000\n");
    }

    #[test]
    fn session_key_is_short_and_sanitised() {
        assert_eq!(session_key("900bfec1-14a3-49aa"), "900bfec1");
        assert_eq!(session_key("../../x"), "x");
        assert_eq!(session_key(""), "local");
    }

    /// Shared buffer so a test can read what the wrapper printed while it runs.
    #[derive(Clone, Default)]
    struct SharedOut(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SharedOut {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn running_command_has_a_handle_and_partial_output_before_it_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cfg(tmp.path());
        let shared = SharedOut::default();
        let mut writer = shared.clone();
        let root = tmp.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            run_wrapped(
                &cfg(&root),
                "kill0001",
                &["echo partial-output; sleep 3; echo late-output".into()],
                true,
                &mut writer,
            )
        });
        // The header appears and the partial output is readable while the
        // command is still running (this is what survives a Bash-tool timeout).
        let deadline = std::time::Instant::now() + Duration::from_millis(2500);
        let mut partial = String::new();
        while std::time::Instant::now() < deadline {
            let header = String::from_utf8(shared.0.lock().unwrap().clone()).unwrap();
            if header.contains("running: echo partial-output")
                && let Ok(text) = show(&c.root, "kill0001/1", None, None)
                && text.contains("partial-output")
            {
                partial = text;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(partial, "partial-output\n");
        assert!(!worker.is_finished(), "the command must still be running");
        let side: Value =
            serde_json::from_str(&std::fs::read_to_string(c.root.join("kill0001/1.json")).unwrap())
                .unwrap();
        assert_eq!(side["running"], true);
        assert_eq!(worker.join().unwrap(), 0);
        let side: Value =
            serde_json::from_str(&std::fs::read_to_string(c.root.join("kill0001/1.json")).unwrap())
                .unwrap();
        assert_eq!(side["running"], false);
        assert_eq!(side["exit"], 0);
        assert_eq!(
            show(&c.root, "kill0001/1", None, None).unwrap(),
            "partial-output\nlate-output\n"
        );
    }

    #[test]
    fn wrapper_path_with_spaces_and_quotes_is_shell_quoted() {
        let routed = rewrite_bash_input(
            &json!({"tool_input": {"command": "cargo build"}}),
            "/home/my user/it's/rsi-rpc",
        )
        .unwrap();
        assert_eq!(
            routed["command"],
            "'/home/my user/it'\\''s/rsi-rpc' spill --shell 'cargo build'"
        );
    }

    #[test]
    fn kill_switch_file_disables_spill_for_the_configured_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_string_lossy().into_owned();
        let lookup = |name: &str| (name == ENV_DIR).then(|| root.clone());
        assert!(!SpillConfig::from_lookup(&lookup).disabled);
        std::fs::write(tmp.path().join(KILL_SWITCH_FILE), "").unwrap();
        assert!(kill_switch_present(tmp.path()));
        let cfg = SpillConfig::from_lookup(&lookup);
        assert!(cfg.disabled);
        let big = json!({"session_id": "x", "tool_name": "Bash", "tool_input": {"command": "ls"},
            "tool_response": {"stdout": "x\n".repeat(5000), "stderr": ""}});
        assert!(spill_tool_response(&big, &cfg).is_none());
        // A disabled wrapper streams raw output with no header and no files.
        let mut out = Vec::new();
        assert_eq!(
            run_wrapped(&cfg, "k", &["echo".into(), "hi".into()], false, &mut out),
            0
        );
        assert_eq!(String::from_utf8(out).unwrap(), "hi\n");
        assert!(!tmp.path().join("k").exists());
    }
}
