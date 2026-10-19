//! The embedded terminal overlay's shells (slice 12, hub #3014).
//!
//! Mirrors the TUI's embedded shell (`crates/rsi/src/terminal.rs`): the user's
//! shell (`$SHELL`, else `/bin/sh`; `%COMSPEC%`, else `cmd.exe` on Windows,
//! through ConPTY) runs in a PTY the backend owns, in the open session's
//! working directory. The webview gets the output as `pty-output` events and
//! the exit as one `pty-exit` event, and drives the shell with `pty_write`,
//! `pty_resize` and `pty_kill`. Several terminals live side by side, keyed by
//! a UUID; every one is killed when the window closes. No daemon method is
//! involved.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

/// Tauri event carrying shell output.
pub const OUTPUT_EVENT: &str = "pty-output";
/// Tauri event sent once when a shell has exited (or was killed).
pub const EXIT_EVENT: &str = "pty-exit";
/// More shells than this at once is a bug in the frontend, not a use.
pub const MAX_TERMINALS: usize = 16;
/// Largest single `pty_write` (a big paste is split by the frontend).
pub const MAX_WRITE_BYTES: usize = 64 * 1024;
pub const MAX_ROWS: u16 = 1000;
pub const MAX_COLS: u16 = 1000;
const READ_CHUNK: usize = 16 * 1024;
const REAP_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PtyOutput {
    pub id: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PtyExit {
    pub id: String,
    /// The shell's exit code; `None` when it could not be read.
    pub code: Option<u32>,
}

/// Where a terminal's output goes: Tauri events in the app, a channel in tests.
pub trait PtySink: Send + Sync + 'static {
    fn output(&self, id: &str, data: String);
    fn exit(&self, id: &str, code: Option<u32>);
}

/// What to run, where, at which size.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
}

/// The shell the overlay runs: `$SHELL` (else `/bin/sh`) on unix, `%COMSPEC%`
/// (else `cmd.exe`) on Windows.
pub fn default_shell() -> String {
    #[cfg(windows)]
    let (var, fallback) = ("COMSPEC", "cmd.exe");
    #[cfg(not(windows))]
    let (var, fallback) = ("SHELL", "/bin/sh");
    shell_from(std::env::var(var).ok().as_deref(), fallback)
}

fn shell_from(var: Option<&str>, fallback: &str) -> String {
    match var.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => fallback.to_string(),
    }
}

fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

/// The shell's directory: the session's working_dir when it is an absolute,
/// existing directory, else the home directory, else the filesystem root.
pub fn resolve_cwd(requested: Option<&str>, home: Option<&Path>) -> PathBuf {
    if let Some(dir) = requested.map(str::trim).filter(|d| !d.is_empty()) {
        let p = Path::new(dir);
        if p.is_absolute() && p.is_dir() {
            return p.to_path_buf();
        }
    }
    if let Some(h) = home.filter(|h| h.is_dir()) {
        return h.to_path_buf();
    }
    PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" })
}

/// Validate a grid size from the webview.
pub fn check_size(rows: u16, cols: u16) -> Result<PtySize, String> {
    if rows == 0 || cols == 0 || rows > MAX_ROWS || cols > MAX_COLS {
        return Err(format!(
            "terminal size {cols}x{rows} is outside 1..={MAX_COLS} columns and 1..={MAX_ROWS} rows"
        ));
    }
    Ok(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })
}

/// Decodes a byte stream as UTF-8 across read boundaries: an incomplete
/// sequence at the end of one chunk waits for the next, an invalid one
/// becomes U+FFFD.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut out = String::with_capacity(buf.len());
        let mut rest: &[u8] = &buf;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    break;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(good).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{FFFD}');
                            rest = &bad[n..];
                        }
                        None => {
                            self.pending = bad.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// Whatever is left at end of stream.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&rest).into_owned()
    }
}

struct Term {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    pid: Option<u32>,
}

/// Every open terminal, keyed by id. Cheap to clone (shared map).
#[derive(Clone, Default)]
pub struct Terminals {
    inner: Arc<Mutex<HashMap<String, Arc<Term>>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Terminals {
    /// Start `spec` in a new PTY and stream its output to `sink`. Returns the id.
    pub fn spawn(&self, spec: &SpawnSpec, sink: Arc<dyn PtySink>) -> Result<String, String> {
        if lock(&self.inner).len() >= MAX_TERMINALS {
            return Err(format!("at most {MAX_TERMINALS} terminals can be open"));
        }
        let size = check_size(spec.rows, spec.cols)?;
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| format!("PTY open failed: {e}"))?;
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("could not start {}: {e}", spec.program))?;
        // Drop the slave so the reader sees EOF once the shell exits.
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("PTY reader failed: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("PTY writer failed: {e}"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let pid = child.process_id();
        let term = Arc::new(Term {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            child: Arc::new(Mutex::new(child)),
            pid,
        });
        let child = term.child.clone();
        lock(&self.inner).insert(id.clone(), term);
        let map = self.clone();
        let tid = id.clone();
        let spawned = std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || pump(&tid, reader, child, sink.as_ref(), &map));
        if let Err(e) = spawned {
            let _ = self.kill(&id);
            return Err(format!("PTY reader thread failed: {e}"));
        }
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Arc<Term>, String> {
        lock(&self.inner)
            .get(id)
            .cloned()
            .ok_or_else(|| "that terminal is not running".to_string())
    }

    pub fn write(&self, id: &str, data: &[u8]) -> Result<(), String> {
        if data.len() > MAX_WRITE_BYTES {
            return Err(format!("terminal input exceeds {MAX_WRITE_BYTES} bytes"));
        }
        let term = self.get(id)?;
        let mut w = lock(&term.writer);
        w.write_all(data)
            .and_then(|()| w.flush())
            .map_err(|e| format!("terminal write failed: {e}"))
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<(), String> {
        let size = check_size(rows, cols)?;
        let term = self.get(id)?;
        lock(&term.master)
            .resize(size)
            .map_err(|e| format!("terminal resize failed: {e}"))
    }

    /// The shell's process id, while it runs.
    pub fn pid(&self, id: &str) -> Option<u32> {
        self.get(id).ok().and_then(|t| t.pid)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = lock(&self.inner).keys().cloned().collect();
        v.sort();
        v
    }

    /// Kill and reap one shell. False when it was not running.
    pub fn kill(&self, id: &str) -> Result<bool, String> {
        let Some(term) = lock(&self.inner).remove(id) else {
            return Ok(false);
        };
        kill_term(&term)?;
        Ok(true)
    }

    /// Kill every shell (window close). Returns how many were running.
    pub fn kill_all(&self) -> usize {
        let all: Vec<Arc<Term>> = lock(&self.inner).drain().map(|(_, t)| t).collect();
        for t in &all {
            let _ = kill_term(t);
        }
        all.len()
    }
}

/// SIGHUP, then SIGKILL after a short grace (portable-pty's `Child::kill`;
/// TerminateProcess on Windows), then wait so no zombie is left.
fn kill_term(term: &Term) -> Result<(), String> {
    let mut child = lock(&term.child);
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Ok(());
    }
    if let Err(e) = child.kill() {
        // Already gone between try_wait and kill.
        if !matches!(child.try_wait(), Ok(Some(_))) {
            return Err(format!("terminal kill failed: {e}"));
        }
    }
    child
        .wait()
        .map(|_| ())
        .map_err(|e| format!("terminal wait failed: {e}"))
}

/// Reader thread: stream output until EOF, then reap the shell and report its exit.
fn pump(
    id: &str,
    mut reader: Box<dyn Read + Send>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    sink: &dyn PtySink,
    map: &Terminals,
) {
    let mut utf8 = Utf8Stream::default();
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let text = utf8.push(&buf[..n]);
                if !text.is_empty() {
                    sink.output(id, text);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // EIO on Linux once the slave side is closed.
            Err(_) => break,
        }
    }
    let tail = utf8.finish();
    if !tail.is_empty() {
        sink.output(id, tail);
    }
    let code = loop {
        let status = lock(&child).try_wait();
        match status {
            Ok(Some(s)) => break Some(s.exit_code()),
            Ok(None) => std::thread::sleep(REAP_POLL),
            Err(_) => break None,
        }
    };
    lock(&map.inner).remove(id);
    sink.exit(id, code);
}

// ---------- Tauri surface ----------

struct EventSink(AppHandle);

impl PtySink for EventSink {
    fn output(&self, id: &str, data: String) {
        let _ = self.0.emit(
            OUTPUT_EVENT,
            PtyOutput {
                id: id.to_string(),
                data,
            },
        );
    }

    fn exit(&self, id: &str, code: Option<u32>) {
        let _ = self.0.emit(
            EXIT_EVENT,
            PtyExit {
                id: id.to_string(),
                code,
            },
        );
    }
}

fn parse_id(id: &str) -> Result<String, String> {
    crate::parse_uuid("terminal id", id)
}

/// Start the user's shell in `cwd` (the open session's working_dir; the home
/// directory when absent or not a directory). Returns the terminal id and the
/// directory it runs in.
#[tauri::command]
pub async fn pty_spawn(
    app: AppHandle,
    terminals: State<'_, Terminals>,
    cwd: Option<String>,
    rows: u16,
    cols: u16,
) -> Result<serde_json::Value, String> {
    let dir = resolve_cwd(cwd.as_deref(), home_dir().as_deref());
    let spec = SpawnSpec {
        program: default_shell(),
        args: Vec::new(),
        cwd: dir.clone(),
        rows,
        cols,
    };
    let terms = terminals.inner().clone();
    let sink: Arc<dyn PtySink> = Arc::new(EventSink(app));
    let id = tauri::async_runtime::spawn_blocking(move || terms.spawn(&spec, sink))
        .await
        .map_err(|e| e.to_string())??;
    Ok(serde_json::json!({ "id": id, "cwd": dir.display().to_string() }))
}

#[tauri::command]
pub async fn pty_write(
    terminals: State<'_, Terminals>,
    id: String,
    data: String,
) -> Result<(), String> {
    terminals.write(&parse_id(&id)?, data.as_bytes())
}

#[tauri::command]
pub async fn pty_resize(
    terminals: State<'_, Terminals>,
    id: String,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    terminals.resize(&parse_id(&id)?, rows, cols)
}

/// Kill a shell. True when it was running.
#[tauri::command]
pub async fn pty_kill(terminals: State<'_, Terminals>, id: String) -> Result<bool, String> {
    let id = parse_id(&id)?;
    let terms = terminals.inner().clone();
    tauri::async_runtime::spawn_blocking(move || terms.kill(&id))
        .await
        .map_err(|e| e.to_string())?
}

/// Window hook (`Builder::on_window_event`): a closed window takes its shells with it.
pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if matches!(event, tauri::WindowEvent::Destroyed) {
        window.state::<Terminals>().kill_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_stream_joins_split_sequences() {
        let mut s = Utf8Stream::default();
        let bytes = "héllo ✓".as_bytes();
        // Split inside the two-byte é and inside the three-byte ✓.
        let a = s.push(&bytes[..2]);
        let b = s.push(&bytes[2..8]);
        let c = s.push(&bytes[8..]);
        assert_eq!(format!("{a}{b}{c}"), "héllo ✓");
        assert_eq!(a, "h");
        assert_eq!(s.finish(), "");
    }

    #[test]
    fn utf8_stream_replaces_invalid_bytes() {
        let mut s = Utf8Stream::default();
        assert_eq!(s.push(b"a\xffb"), "a\u{FFFD}b");
        assert_eq!(s.push(b"\xe2\x9c"), "");
        assert_eq!(s.finish(), "\u{FFFD}");
    }

    #[test]
    fn size_is_bounded() {
        assert!(check_size(24, 80).is_ok());
        assert!(check_size(0, 80).is_err());
        assert!(check_size(24, 0).is_err());
        assert!(check_size(MAX_ROWS + 1, 80).is_err());
        assert!(check_size(24, MAX_COLS + 1).is_err());
    }

    #[test]
    fn shell_falls_back_when_unset() {
        assert_eq!(shell_from(Some("/bin/zsh"), "/bin/sh"), "/bin/zsh");
        assert_eq!(shell_from(Some("  "), "/bin/sh"), "/bin/sh");
        assert_eq!(shell_from(None, "/bin/sh"), "/bin/sh");
    }

    #[test]
    fn cwd_prefers_an_existing_absolute_session_dir() {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let w = work.path().to_str().unwrap();
        assert_eq!(resolve_cwd(Some(w), Some(home.path())), work.path());
        assert_eq!(
            resolve_cwd(Some("relative/dir"), Some(home.path())),
            home.path()
        );
        let gone = work.path().join("missing");
        assert_eq!(resolve_cwd(gone.to_str(), Some(home.path())), home.path());
        assert_eq!(resolve_cwd(None, Some(home.path())), home.path());
        assert!(resolve_cwd(None, None).is_absolute());
    }

    #[test]
    fn ids_must_be_uuids() {
        assert!(parse_id("../x").is_err());
        assert!(parse_id("E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE").is_ok());
    }

    #[cfg(unix)]
    mod unix {
        use super::super::*;
        use std::sync::mpsc;
        use std::time::Instant;

        enum Msg {
            Out(String),
            Exit(Option<u32>),
        }

        struct ChanSink(Mutex<mpsc::Sender<Msg>>);

        impl PtySink for ChanSink {
            fn output(&self, _id: &str, data: String) {
                let _ = lock(&self.0).send(Msg::Out(data));
            }
            fn exit(&self, _id: &str, code: Option<u32>) {
                let _ = lock(&self.0).send(Msg::Exit(code));
            }
        }

        fn start(terms: &Terminals, script: &str) -> (String, mpsc::Receiver<Msg>) {
            let (tx, rx) = mpsc::channel();
            let spec = SpawnSpec {
                program: "sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: std::env::temp_dir(),
                rows: 24,
                cols: 80,
            };
            let id = terms
                .spawn(&spec, Arc::new(ChanSink(Mutex::new(tx))))
                .unwrap();
            (id, rx)
        }

        /// All output until the exit event (or until `until` shows up, when given).
        fn collect(rx: &mpsc::Receiver<Msg>, until: Option<&str>) -> (String, Option<Option<u32>>) {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut out = String::new();
            while Instant::now() < deadline {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Msg::Out(s)) => {
                        out.push_str(&s);
                        if until.is_some_and(|u| out.contains(u)) {
                            return (out, None);
                        }
                    }
                    Ok(Msg::Exit(code)) => return (out, Some(code)),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            panic!("timed out; output so far: {out:?}");
        }

        fn alive(pid: u32) -> bool {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        #[test]
        fn echo_hi_streams_and_exits_zero() {
            let terms = Terminals::default();
            let (id, rx) = start(&terms, "echo hi");
            let (out, exit) = collect(&rx, None);
            assert!(out.contains("hi"), "{out:?}");
            assert_eq!(exit, Some(Some(0)));
            assert!(!terms.ids().contains(&id), "an exited shell leaves the map");
        }

        #[test]
        fn exit_code_is_reported() {
            let terms = Terminals::default();
            let (_id, rx) = start(&terms, "exit 3");
            assert_eq!(collect(&rx, None).1, Some(Some(3)));
        }

        #[test]
        fn write_reaches_the_shell() {
            let terms = Terminals::default();
            let (id, rx) = start(&terms, "read line; echo got:$line");
            terms.write(&id, b"abc\r").unwrap();
            let (out, exit) = collect(&rx, None);
            assert!(out.contains("got:abc"), "{out:?}");
            assert_eq!(exit, Some(Some(0)));
        }

        #[test]
        fn resize_changes_the_tty_size() {
            let terms = Terminals::default();
            let (id, rx) = start(&terms, "read go; stty size");
            terms.resize(&id, 40, 100).unwrap();
            terms.write(&id, b"\r").unwrap();
            let (out, _) = collect(&rx, None);
            assert!(out.contains("40 100"), "{out:?}");
        }

        #[test]
        fn kill_reaps_the_process() {
            let terms = Terminals::default();
            let (id, rx) = start(&terms, "echo ready; sleep 30");
            collect(&rx, Some("ready"));
            let pid = terms.pid(&id).expect("pid while running");
            assert!(alive(pid));
            assert!(terms.kill(&id).unwrap(), "it was running");
            // Reaped by kill: no zombie answers kill -0.
            assert!(!alive(pid), "pid {pid} still exists after kill");
            let (_, exit) = collect(&rx, None);
            assert!(exit.is_some(), "the exit event still arrives");
            assert!(!terms.ids().contains(&id));
            assert!(!terms.kill(&id).unwrap(), "a second kill is a no-op");
            assert!(terms.write(&id, b"x").is_err());
        }

        #[test]
        fn kill_all_ends_every_shell() {
            let terms = Terminals::default();
            let (a, ra) = start(&terms, "echo a-up; sleep 30");
            let (b, rb) = start(&terms, "echo b-up; sleep 30");
            collect(&ra, Some("a-up"));
            collect(&rb, Some("b-up"));
            let pids = [terms.pid(&a).unwrap(), terms.pid(&b).unwrap()];
            assert_eq!(terms.kill_all(), 2);
            assert!(terms.ids().is_empty());
            for p in pids {
                assert!(!alive(p), "pid {p} survived kill_all");
            }
        }

        #[test]
        fn spawn_refuses_a_bad_size_and_an_unknown_id() {
            let terms = Terminals::default();
            let (tx, _rx) = mpsc::channel();
            let spec = SpawnSpec {
                program: "sh".into(),
                args: vec![],
                cwd: std::env::temp_dir(),
                rows: 0,
                cols: 80,
            };
            assert!(
                terms
                    .spawn(&spec, Arc::new(ChanSink(Mutex::new(tx))))
                    .is_err()
            );
            assert!(terms.write("nope", b"x").is_err());
            assert!(terms.resize("nope", 10, 10).is_err());
        }
    }
}
