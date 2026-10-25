//! Ctrl-C never quits rsi and never reaches the daemon (#3434). Each test
//! drives the production key path (`event::step_once`) and asserts the end
//! state: the app is still running and the hint names the proper exit.

use crate::app::App;
use crate::event::step_once;
use crate::key_tables::{GLOBAL_KEY_INTERCEPTS, GlobalEffect, GlobalGuard, lookup};
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const HINT: &str = "Ctrl-C does not quit rsi. Press Esc, then :q (or ZZ) to quit";

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn hint_shown(app: &App) -> bool {
    app.notifications.iter().any(|n| n.message == HINT)
}

#[tokio::test]
async fn ctrl_c_in_the_session_list_shows_the_exit_hint() {
    let mut app = crate::app::app_test_helpers::with_session_list(2);
    step_once(&mut app, ctrl_c()).await;
    assert!(!app.quit, "Ctrl-C must not quit the session list");
    assert!(hint_shown(&app), "the hint names :q and ZZ");
}

#[tokio::test]
async fn ctrl_c_in_an_overlay_keeps_it_open_and_shows_the_hint() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    crate::overlay::command_palette::open_command_palette(&mut app);
    step_once(&mut app, ctrl_c()).await;
    assert!(!app.quit, "Ctrl-C must not quit from an overlay");
    assert!(matches!(app.overlay, OverlayState::CommandPalette { .. }));
    assert!(hint_shown(&app));
}

#[tokio::test]
async fn ctrl_c_in_a_text_field_without_selection_shows_the_hint() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    crate::settings::DaemonFeatureEntry::update_from_json(
        &mut app.daemon_features,
        &serde_json::json!({ "editing_mode": "standard" }),
    );
    crate::overlay::command_palette::open_command_palette(&mut app);
    for c in "abc".chars() {
        step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
        )
        .await;
    }
    step_once(&mut app, ctrl_c()).await;
    assert!(!app.quit, "Ctrl-C must not quit from a text field");
    assert!(hint_shown(&app));
}

#[test]
fn the_embedded_terminal_keeps_its_interrupt_and_nothing_else_quits() {
    let in_terminal = lookup(GLOBAL_KEY_INTERCEPTS, ctrl_c(), |guard| {
        matches!(guard, GlobalGuard::TerminalOverlay | GlobalGuard::Always)
    })
    .map(|entry| entry.effect);
    assert_eq!(in_terminal, Some(GlobalEffect::InterruptTerminal));
    let elsewhere = lookup(GLOBAL_KEY_INTERCEPTS, ctrl_c(), |guard| {
        matches!(guard, GlobalGuard::Always)
    })
    .map(|entry| entry.effect);
    assert_eq!(elsewhere, Some(GlobalEffect::CtrlCHint));
}

#[tokio::test]
async fn ctrl_c_in_the_terminal_overlay_does_not_quit() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    app.overlay = OverlayState::Terminal;
    step_once(&mut app, ctrl_c()).await;
    assert!(!app.quit);
    assert!(matches!(app.overlay, OverlayState::Terminal));
}

#[test]
fn the_proper_exit_commands_still_quit() {
    use crate::commands::{CommandResult, parse_command};
    use crate::modalkit_types::LcAction;
    for command in ["q", "quit", "q!"] {
        assert_eq!(
            parse_command(command),
            CommandResult::LcAction(LcAction::Quit),
            ":{command}"
        );
    }
}

/// No code in the TUI crate may signal a process by pid or process group: the
/// daemon stops only through its explicit RPC or the operator's Stop action,
/// never because a TUI key or exit path sent it a signal. The embedded shell is
/// reached through its pty (0x03 written to the master), not a signal.
#[test]
fn no_tui_source_can_signal_a_daemon_pid() {
    let forbidden = [
        concat!("libc::", "kill"),
        concat!("kill", "pg"),
        concat!("signal::", "kill"),
        concat!("Command::new(\"", "kill\")"),
        concat!("Command::new(\"", "pkill\")"),
        concat!("\"", "pkill\""),
    ];
    let root = std::path::Path::new(rsi_common::manifest_dir!()).join("src");
    let mut stack = vec![root];
    let mut scanned = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            scanned += 1;
            for token in forbidden {
                assert!(
                    !text.contains(token),
                    "{} signals a process via `{token}`; the TUI must never signal the daemon",
                    path.display()
                );
            }
        }
    }
    assert!(scanned > 20, "the scan walked the TUI sources ({scanned})");
}

#[test]
fn the_hint_names_q_for_the_first_start_prompt() {
    let mut app = crate::app::app_test_helpers::with_session_list(0);
    assert_eq!(crate::event::ctrl_c_hint(&app), HINT);
    app.editing_mode_prompt = Some(crate::editing_mode_prompt::EditingModePrompt::default());
    assert_eq!(
        crate::event::ctrl_c_hint(&app),
        "Ctrl-C does not quit rsi. Choose a mode, or press q to quit"
    );
}
