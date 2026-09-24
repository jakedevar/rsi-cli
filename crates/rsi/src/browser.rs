//! Best-effort "open a URL in the default browser" helper.
//!
//! No `open`/`opener`/`webbrowser` crate dependency — mirrors `clipboard.rs`'s
//! preference for a narrowly-scoped, zero-dependency process spawn over a new
//! crate for a single OS interaction.

use std::process::{Command, Stdio};

/// Attempt to open `url` in the user's default browser (or other registered
/// handler for its scheme, e.g. `mailto:`).
///
/// Spawns the platform's standard opener command and does not wait for it —
/// a right-click should never block the TUI on a subprocess. Success here
/// means the opener process launched, not that a browser window actually
/// appeared.
///
/// # Errors
///
/// Returns `Err` if the opener command itself could not be spawned (e.g.
/// `xdg-open` missing on a minimal Linux install).
pub fn open_url(url: &str) -> std::io::Result<()> {
    let mut cmd = opener_command(url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn opener_command(url: &str) -> Command {
    let mut cmd = Command::new("open");
    cmd.arg(url);
    cmd
}

#[cfg(target_os = "windows")]
fn opener_command(url: &str) -> Command {
    // `cmd /C start "" <url>` — the empty title arg keeps `start` from
    // treating a quoted URL as the window title.
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "start", "", url]);
    cmd
}

#[cfg(all(unix, not(target_os = "macos")))]
fn opener_command(url: &str) -> Command {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(url);
    cmd
}
