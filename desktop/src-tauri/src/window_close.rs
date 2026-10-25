//! What closing the window means (local #311). On macOS an app stays running when its last window closes: Cmd-W,
//! Window > Close Window and the red button hide the window (the page, its open dialogs, its drafts and the terminal
//! shells stay alive behind it), a Dock click brings it back (`RunEvent::Reopen`) and only Cmd-Q, File > Quit, `ZQ`,
//! `ZZ` and `:quit` end the process. On Linux and Windows closing the last window quits, as before.
//! Purely local: nothing goes to the daemon.

use tauri::{AppHandle, Manager, RunEvent, Runtime, Window, WindowEvent};

/// What a close request on the window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Keep the window (and the app) alive, only hidden.
    Hide,
    /// Let the window close; closing the last window quits the app.
    Quit,
}

/// The close policy of a platform, named by `std::env::consts::OS`: macOS hides, everything else quits.
pub fn close_action(os: &str) -> CloseAction {
    if os == "macos" {
        CloseAction::Hide
    } else {
        CloseAction::Quit
    }
}

/// Window hook (called from `Builder::on_window_event`): turn a close request into a hide where the policy says so.
pub fn on_window_event<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    if let WindowEvent::CloseRequested { api, .. } = event
        && close_action(std::env::consts::OS) == CloseAction::Hide
    {
        api.prevent_close();
        let _ = window.hide();
    }
}

/// Show the app's windows again and focus the first (Dock click, or a menu choice while hidden).
pub fn reveal<R: Runtime>(app: &AppHandle<R>) {
    let windows = app.webview_windows();
    for w in windows.values() {
        let _ = w.unminimize();
        let _ = w.show();
    }
    if let Some(w) = windows.values().next() {
        let _ = w.set_focus();
    }
}

/// True when the app has a window and none is visible: the state a hide leaves behind.
pub fn all_hidden<R: Runtime>(app: &AppHandle<R>) -> bool {
    let windows = app.webview_windows();
    !windows.is_empty() && windows.values().all(|w| !w.is_visible().unwrap_or(true))
}

/// `App::run` callback: a Dock click shows the hidden window; a real exit ends the terminal shells (a hide must not,
/// and `app.exit` destroys no window, so `pty::on_window_event` alone would miss them).
pub fn on_run_event<R: Runtime>(app: &AppHandle<R>, event: RunEvent) {
    match event {
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => reveal(app),
        RunEvent::Exit => {
            if let Some(terms) = app.try_state::<crate::pty::Terminals>() {
                terms.kill_all();
            }
        }
        _ => {}
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_hides_on_close_everything_else_quits() {
        assert_eq!(close_action("macos"), CloseAction::Hide);
        for os in ["linux", "windows", "freebsd", "ios", ""] {
            assert_eq!(close_action(os), CloseAction::Quit, "{os}");
        }
    }

    #[test]
    fn the_running_platform_follows_the_policy() {
        let expected = if cfg!(target_os = "macos") {
            CloseAction::Hide
        } else {
            CloseAction::Quit
        };
        assert_eq!(close_action(std::env::consts::OS), expected);
    }
}
