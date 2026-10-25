//! #3162: a real pseudo-terminal for `exec_command` with `tty: true`.
//!
//! The scoped command (the local `systemd-run` scope around `bash -c`, or the
//! `docker exec -t` client of a container target) is built exactly as for the
//! pipe path and then launched on a PTY of a fixed size instead of three pipes.
//! The child becomes a session leader with the terminal as its controlling
//! terminal, so it is its own process group (the group the supervisor signals)
//! and `isatty` holds for it. Output of the terminal is one stream; a `^C` or
//! `^D` byte written to it is interpreted by the terminal line discipline.

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, PoisonError};

/// Fixed terminal size of every PTY handle.
pub(super) const PTY_ROWS: u16 = 24;
pub(super) const PTY_COLS: u16 = 80;
/// `TERM` of every PTY handle.
pub(super) const PTY_TERM: &str = "xterm-256color";
/// The interrupt character (`^C`): the terminal turns it into `SIGINT` for the
/// foreground process group.
pub(super) const CTRL_C: u8 = 0x03;
/// The end-of-file character (`^D`): at the start of a line the foreground
/// reader sees EOF.
pub(super) const CTRL_D: u8 = 0x04;

/// A command launched on a fresh pseudo-terminal.
pub(super) struct PtyLaunch {
    pub(super) child: Box<dyn portable_pty::Child + Send + Sync>,
    pub(super) pid: i32,
    pub(super) io: PtyIo,
    pub(super) reader: Box<dyn Read + Send>,
}

/// The master side a handle keeps: the writer the model's input goes to and the
/// master itself, so the terminal stays open for as long as the handle lives.
pub(super) struct PtyIo {
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
}

impl std::fmt::Debug for PtyIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyIo").finish_non_exhaustive()
    }
}

impl PtyIo {
    /// Write `bytes` to the terminal. The write can block on a child that does
    /// not read, so it runs off the async workers.
    pub(super) async fn write(&self, bytes: Vec<u8>) -> Result<(), String> {
        let writer = Arc::clone(&self.writer);
        tokio::task::spawn_blocking(move || {
            let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(writer) = writer.as_mut() else {
                return Err("the terminal is closed".to_string());
            };
            writer
                .write_all(&bytes)
                .and_then(|()| writer.flush())
                .map_err(|error| format!("failed to write the terminal: {error}"))
        })
        .await
        .map_err(|error| format!("terminal write task failed: {error}"))?
    }

    /// Close the terminal. The master is dropped once the output has drained.
    pub(super) fn release(&self) {
        self.writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.master
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// Launch the configured `command` on a new PTY. Its program, arguments,
/// working directory and explicit environment carry over; the environment is
/// cleared first and `TERM` is set.
pub(super) fn spawn(command: &std::process::Command) -> Result<PtyLaunch, String> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: PTY_ROWS,
            cols: PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("tty_unavailable: failed to open a pseudo-terminal: {error}"))?;
    let mut builder = CommandBuilder::new(command.get_program());
    builder.args(command.get_args());
    if let Some(dir) = command.get_current_dir() {
        builder.cwd(dir);
    }
    builder.env_clear();
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => builder.env(key, value),
            None => builder.env_remove(key),
        }
    }
    builder.env("TERM", PTY_TERM);
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| format!("tty_unavailable: failed to start on the terminal: {error}"))?;
    // The child holds the only slave end; the output reader sees EOF when it
    // and its descendants are gone.
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("tty_unavailable: failed to read the terminal: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("tty_unavailable: failed to write the terminal: {error}"))?;
    let pid = child
        .process_id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| "spawned process did not expose a pid".to_string())?;
    Ok(PtyLaunch {
        child,
        pid,
        io: PtyIo {
            writer: Arc::new(Mutex::new(Some(writer))),
            master: Mutex::new(Some(pair.master)),
        },
        reader,
    })
}
