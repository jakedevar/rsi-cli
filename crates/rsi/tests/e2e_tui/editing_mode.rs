//! #1628 slice 4: Termwright E2E for the `editing_mode` setting.
//!
//! * the first-start prompt on a session-less fresh daemon (both choices; Esc
//!   never dismisses it; the choice lands in the daemon setting and survives a
//!   TUI restart),
//! * the chat composer and one overlay (the command palette) in Standard and
//!   in Vim,
//! * a live switch of the setting through Settings, without a restart.
//!
//! Fixture data only. The composer scenarios send a message to a completed
//! Codex stub session; the exact text the daemon recorded proves the edits.

use super::*;
use rsi::settings_registry::{SettingsGroup, SettingsSection};
use rsi_common::types::Role;

const SESSION_TITLE: &str = "EDITING MODE E2E";
const PROMPT_TITLE: &str = "Choose how you edit text";
const COLS: u16 = 120;
const ROWS: u16 = 40;

// Chords termwright's `Key` cannot express; crossterm decodes these CSI forms.
const CTRL_LEFT: &[u8] = b"\x1b[1;5D";
const SHIFT_END: &[u8] = b"\x1b[1;2F";
// The TUI asks for the kitty keyboard protocol, where Ctrl-H is distinct from
// Backspace (0x08 would be read as Backspace).
const CTRL_H: &[u8] = b"\x1b[104;5u";

fn screen_error(what: impl std::fmt::Display, screen: &str) -> Box<dyn Error + Send + Sync> {
    test_error(format!("{what}; screen:\n{screen}"))
}

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    let label = format!("{key:?}");
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|error| test_error(format!("key {label}: {error:?}")))
}

async fn raw(term: &Terminal, bytes: &[u8]) -> E2eResult<()> {
    term.send_raw(bytes)
        .await
        .map(|_| ())
        .map_err(|error| test_error(format!("raw {bytes:?}: {error:?}")))
}

async fn typed(term: &Terminal, text: &str) -> E2eResult<()> {
    term.type_str(text)
        .await
        .map(|_| ())
        .map_err(|error| test_error(format!("type {text:?}: {error:?}")))
}

async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    if let Err(error) = term.expect(text).timeout(Duration::from_secs(8)).await {
        let screen = term.screen().await.text();
        return Err(screen_error(
            format!("expected {text:?}: {error:?}"),
            &screen,
        ));
    }
    Ok(())
}

async fn settled(term: &Terminal) {
    tokio::time::sleep(Duration::from_millis(400)).await;
    let _ = term;
}

/// Spawn the TUI against the harness daemon.
async fn spawn_tui(h: &E2eHarness) -> E2eResult<Terminal> {
    Terminal::builder()
        .size(COLS, ROWS)
        .env("HOME", h.path_string(&h.home_dir)?)
        .env("RSI_DAEMON_SOCKET_PATH", h.path_string(&h.socket_path)?)
        .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
        .env("PATH", &h.isolated_path)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("LANG", "C.UTF-8")
        .env("TZ", "UTC")
        .spawn(&h.rsi_bin, &[])
        .await
        .map_err(|error| test_error(format!("failed to spawn rsi: {error:?}")))
}

/// The shared Codex stub finishes a turn but leaves no rollout file, so the
/// daemon refuses to continue that session. Give the stub a rollout to resume.
fn make_codex_stub_resumable(h: &E2eHarness) -> E2eResult<()> {
    let path = h.bin_dir.join("codex");
    let script = fs::read_to_string(&path)?;
    let marker = "  printf '{\"type\":\"thread.started\"";
    if !script.contains(marker) {
        return Err(test_error(
            "codex stub changed; resumable patch no longer applies",
        ));
    }
    let rollout = concat!(
        "  mkdir -p \"$CODEX_HOME/sessions/2026\"\n",
        "  printf '%s\\n' '{\"type\":\"session_meta\",\"payload\":{}}' >> ",
        "\"$CODEX_HOME/sessions/2026/rollout-2026-10-07T00-00-00-$thread_id.jsonl\"\n",
    );
    fs::write(
        &path,
        script.replacen(marker, &format!("{rollout}{marker}"), 1),
    )?;
    Ok(())
}

async fn start_daemon(h: &mut E2eHarness) -> E2eResult<DaemonClient> {
    h.phase = "editing-mode-daemon";
    make_codex_stub_resumable(h)?;
    h.start_daemon()?;
    h.wait_for_daemon().await
}

fn find_field(value: &serde_json::Value, field: &str) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map
            .get(field)
            .cloned()
            .or_else(|| map.values().find_map(|inner| find_field(inner, field))),
        serde_json::Value::Array(items) => items.iter().find_map(|inner| find_field(inner, field)),
        _ => None,
    }
}

/// The `editing_mode` value the daemon reports to a fresh operator client.
async fn daemon_editing_mode(h: &E2eHarness) -> E2eResult<String> {
    let mut client = DaemonClient::new(h.socket_path.clone());
    client
        .connect()
        .await
        .map_err(|error| test_error(format!("connect for config: {error}")))?;
    let config = client
        .get_daemon_config()
        .await
        .map_err(|error| test_error(format!("GetDaemonConfig: {error}")))?;
    client.disconnect();
    find_field(&config, "editing_mode")
        .and_then(|value| value.as_str().map(str::to_string))
        .ok_or_else(|| test_error(format!("no editing_mode in config: {config}")))
}

async fn wait_daemon_editing_mode(h: &E2eHarness, want: &str) -> E2eResult<()> {
    let mut last = String::new();
    for _ in 0..60 {
        last = daemon_editing_mode(h).await?;
        if last == want {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(test_error(format!(
        "daemon editing_mode stayed {last:?}, wanted {want:?}"
    )))
}

/// Launch the fixture Codex session and wait for it to finish its first turn.
async fn seed_session(client: &mut DaemonClient) -> E2eResult<uuid::Uuid> {
    let working_dir = std::env::current_dir()?;
    client
        .launch_session_with_opts(
            "report deterministic context capacity",
            Some(SESSION_TITLE),
            Some(&working_dir),
            SessionProvider::Codex,
            Some("gpt-6-astra"),
            None,
            Some(SessionKind::Standard),
            None,
            Some(0),
            Some("low"),
            None,
            None,
            &[TAG.to_string()],
            None,
            None,
        )
        .await
        .map_err(|error| test_error(format!("launch fixture session: {error}")))?;
    for _ in 0..200 {
        let found = client
            .list_sessions()
            .await
            .map_err(|error| test_error(format!("list sessions: {error}")))?
            .into_iter()
            .find(|session| session.title.as_deref() == Some(SESSION_TITLE));
        if let Some(session) = found
            && matches!(
                session.status,
                SessionStatus::Completed | SessionStatus::Failed
            )
        {
            return Ok(session.id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(test_error("fixture session never finished its first turn"))
}

/// The exact text of the user messages the daemon recorded for the session.
async fn user_messages(h: &E2eHarness, session: uuid::Uuid) -> E2eResult<Vec<String>> {
    let mut client = DaemonClient::new(h.socket_path.clone());
    client
        .connect()
        .await
        .map_err(|error| test_error(format!("connect for conversation: {error}")))?;
    let events = client
        .get_conversation(session, None)
        .await
        .map_err(|error| test_error(format!("GetConversation: {error}")))?;
    client.disconnect();
    Ok(events
        .into_iter()
        .filter(|event| event.role == Some(Role::User))
        .map(|event| event.content)
        .collect())
}

async fn expect_user_message(h: &E2eHarness, session: uuid::Uuid, want: &str) -> E2eResult<()> {
    let mut seen = Vec::new();
    for _ in 0..80 {
        seen = user_messages(h, session).await?;
        if seen.iter().any(|message| message.trim() == want) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(test_error(format!(
        "daemon never recorded user message {want:?}; saw {seen:?}"
    )))
}

/// Preserve artifacts and tear everything down; the outcome decides the result.
async fn finish(mut h: E2eHarness, term: &Terminal, outcome: E2eResult<()>) -> E2eResult<()> {
    h.final_screen_text = Some(term.screen().await.text());
    let _ = term.kill().await;
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; editing mode artifacts: {}",
            artifacts.display()
        )));
    }
    Ok(())
}

fn e2e_enabled() -> bool {
    std::env::var("RSI_E2E").unwrap_or_default() == "1"
}

// ---------------------------------------------------------------------------
// First-start prompt
// ---------------------------------------------------------------------------

/// Esc and stray keys never dismiss the prompt; the screen still shows it.
async fn prove_prompt_is_required(term: &Terminal) -> E2eResult<()> {
    visible(term, PROMPT_TITLE).await?;
    visible(term, "Recommended if you are not").await?;
    visible(term, "The mode RSI was built").await?;
    let screen = term.screen().await.text();
    let button_row = screen
        .lines()
        .find(|line| line.contains("Standard") && line.contains("Vim") && line.contains("] "))
        .ok_or_else(|| screen_error("no button row", &screen))?;
    let standard_at = button_row.find("Standard");
    let vim_at = button_row.find("Vim");
    if standard_at >= vim_at {
        return Err(screen_error("Standard must be the left button", &screen));
    }
    press(term, Key::Escape).await?;
    press(term, Key::Char('x')).await?;
    press(term, Key::Escape).await?;
    settled(term).await;
    visible(term, PROMPT_TITLE).await
}

#[tokio::test]
async fn test_e2e_editing_mode_first_start_standard_by_click() -> E2eResult<()> {
    if !e2e_enabled() {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let mut client = start_daemon(&mut h).await?;
    client.disconnect();
    // No sessions are seeded: the existing-install rule must not fire.
    let term = spawn_tui(&h).await?;
    let outcome = async {
        prove_prompt_is_required(&term).await?;
        let unset = daemon_editing_mode(&h).await?;
        if unset != "unset" {
            return Err(test_error(format!(
                "an unanswered prompt must leave the setting unset, got {unset:?}"
            )));
        }
        // Click the Standard button on the screen it was rendered to.
        let screen = term.screen().await;
        let row = (0..ROWS)
            .find(|row| {
                screen
                    .line(*row)
                    .is_some_and(|line| line.contains("[s] Standard"))
            })
            .ok_or_else(|| screen_error("no Standard button row", &screen.text()))?;
        let line = screen.line(row).unwrap_or_default();
        let col = line
            .chars()
            .collect::<Vec<_>>()
            .windows("[s] Standard".chars().count())
            .position(|window| window.iter().collect::<String>() == "[s] Standard")
            .ok_or_else(|| screen_error("no Standard button column", &screen.text()))?;
        term.mouse_click(row, u16::try_from(col + 3)?, MouseButton::Left)
            .await
            .map_err(|error| test_error(format!("click Standard: {error:?}")))?;
        wait_daemon_editing_mode(&h, "standard").await?;
        visible(&term, "Sessions").await?;
        // The choice persists: a second TUI against the same daemon asks nothing.
        let _ = term.kill().await;
        let again = spawn_tui(&h).await?;
        let second = async {
            visible(&again, "Sessions").await?;
            settled(&again).await;
            let screen = again.screen().await.text();
            if screen.contains(PROMPT_TITLE) {
                return Err(screen_error("prompt came back after answering", &screen));
            }
            Ok(())
        }
        .await;
        let _ = again.kill().await;
        second
    }
    .await;
    finish(h, &term, outcome).await
}

#[tokio::test]
async fn test_e2e_editing_mode_first_start_vim_by_keys() -> E2eResult<()> {
    if !e2e_enabled() {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let mut client = start_daemon(&mut h).await?;
    client.disconnect();
    let term = spawn_tui(&h).await?;
    let outcome = async {
        prove_prompt_is_required(&term).await?;
        // Standard is focused first; Right moves to Vim, Enter chooses it.
        press(&term, Key::Right).await?;
        press(&term, Key::Enter).await?;
        wait_daemon_editing_mode(&h, "vim").await?;
        visible(&term, "Sessions").await?;
        settled(&term).await;
        let screen = term.screen().await.text();
        if screen.contains(PROMPT_TITLE) {
            return Err(screen_error("prompt stayed after choosing Vim", &screen));
        }
        Ok(())
    }
    .await;
    finish(h, &term, outcome).await
}

// ---------------------------------------------------------------------------
// Composer and one overlay, both modes
// ---------------------------------------------------------------------------

/// Run the palette check: type `hlp`, step back twice, insert `e`.
/// Standard has a real cursor, so the query becomes `help`. Vim's palette is
/// append-only, so the `e` lands at the end of `hlp`.
async fn palette_edit(term: &Terminal, standard: bool) -> E2eResult<()> {
    typed(term, ":").await?;
    visible(term, "Commands").await?;
    typed(term, "hlp").await?;
    press(term, Key::Left).await?;
    press(term, Key::Left).await?;
    typed(term, "e").await?;
    press(term, Key::End).await?;
    if standard {
        // The query is now "help" and the palette lists the help command.
        visible(term, "Contextual help").await?;
    } else {
        visible(term, "hlpe").await?;
    }
    press(term, Key::Escape).await?;
    Ok(())
}

async fn run_composer_scenario(mode: &'static str) -> E2eResult<()> {
    let standard = mode == "standard";
    let mut h = E2eHarness::new()?;
    let mut client = start_daemon(&mut h).await?;
    let session = seed_session(&mut client).await?;
    client
        .update_daemon_config("editing_mode", serde_json::json!(mode))
        .await
        .map_err(|error| test_error(format!("preset editing_mode: {error}")))?;
    client.disconnect();
    let term = spawn_tui(&h).await?;
    let outcome = async {
        visible(&term, SESSION_TITLE).await?;
        // `i` on the session list focuses the composer.
        typed(&term, "i").await?;
        if standard {
            visible(&term, "EDIT").await?;
            typed(&term, "hello world").await?;
            visible(&term, "hello world").await?;
            // Ctrl-Left: back over "world"; the insert lands between the words.
            raw(&term, CTRL_LEFT).await?;
            typed(&term, "big ").await?;
            visible(&term, "hello big world").await?;
            // Shift-End selects "world"; Ctrl-C copies it and keeps the TUI
            // (and the selection) alive, then typing replaces the selection.
            raw(&term, SHIFT_END).await?;
            press(&term, Key::Ctrl('c')).await?;
            settled(&term).await;
            if term.has_exited().await {
                return Err(test_error("Ctrl-C with a selection quit the TUI"));
            }
            typed(&term, "there").await?;
            visible(&term, "hello big there").await?;
            press(&term, Key::Enter).await?;
            expect_user_message(&h, session, "hello big there").await?;
        } else {
            visible(&term, "INSERT").await?;
            typed(&term, "hello world").await?;
            visible(&term, "hello world").await?;
            // Esc leaves insert for the modal NORMAL state; `b` steps back a
            // word and `i` inserts there.
            press(&term, Key::Escape).await?;
            visible(&term, "NORMAL").await?;
            typed(&term, "bi").await?;
            visible(&term, "INSERT").await?;
            typed(&term, "big ").await?;
            visible(&term, "hello big world").await?;
            press(&term, Key::Enter).await?;
            expect_user_message(&h, session, "hello big world").await?;
        }
        // One overlay: the command palette, from the session list.
        if standard {
            // Esc is the plain-terminal way out of the composer: with nothing
            // selected it returns to the session list. The proof is that `:`
            // now opens the palette instead of typing into the draft.
            typed(&term, "draft").await?;
            visible(&term, "draft").await?;
            press(&term, Key::Escape).await?;
            settled(&term).await;
            palette_edit(&term, true).await?;
            // Ctrl-H (kitty protocol) is the second way out.
            typed(&term, "i").await?;
            visible(&term, "EDIT").await?;
            raw(&term, CTRL_H).await?;
            settled(&term).await;
            palette_edit(&term, true).await?;
        } else {
            press(&term, Key::Escape).await?;
            settled(&term).await;
            palette_edit(&term, false).await?;
        }
        Ok(())
    }
    .await;
    finish(h, &term, outcome).await
}

#[tokio::test]
async fn test_e2e_editing_mode_standard_composer_and_palette() -> E2eResult<()> {
    if !e2e_enabled() {
        return Ok(());
    }
    run_composer_scenario("standard").await
}

#[tokio::test]
async fn test_e2e_editing_mode_vim_composer_and_palette() -> E2eResult<()> {
    if !e2e_enabled() {
        return Ok(());
    }
    run_composer_scenario("vim").await
}

// ---------------------------------------------------------------------------
// Live switch through Settings
// ---------------------------------------------------------------------------

async fn open_editing_mode_setting(term: &Terminal) -> E2eResult<()> {
    press(term, Key::Char(' ')).await?;
    press(term, Key::Char(',')).await?;
    visible(term, "Settings").await?;
    let section = SettingsSection::EditingMode;
    let group = section.group();
    let group_index = SettingsGroup::ALL
        .iter()
        .position(|candidate| *candidate == group)
        .ok_or_else(|| test_error("Editing Mode group missing from registry"))?;
    let tab_index = SettingsSection::ALL
        .iter()
        .filter(|candidate| candidate.group() == group)
        .position(|candidate| *candidate == section)
        .ok_or_else(|| test_error("Editing Mode tab missing from its group"))?;
    // A reopened Settings remembers its last tab; only walk the rail when the
    // breadcrumb does not already name the Editing Mode page.
    settled(term).await;
    if !term.screen().await.text().contains("›  Editing Mode") {
        for _ in 0..group_index {
            press(term, Key::Char('j')).await?;
        }
        for _ in 0..tab_index {
            press(term, Key::Char(']')).await?;
        }
    }
    press(term, Key::Char('h')).await?;
    press(term, Key::Char('l')).await?;
    // Wait for the one-row page itself before touching the row.
    visible(term, "Standard or Vim text editing in every input.").await?;
    visible(term, "▌ Editing mode").await
}

/// Cycle the setting (`unset` -> `standard` -> `vim`) until the daemon holds `want`.
async fn cycle_editing_mode_to(h: &E2eHarness, term: &Terminal, want: &str) -> E2eResult<()> {
    for _ in 0..3 {
        if daemon_editing_mode(h).await? == want {
            return Ok(());
        }
        press(term, Key::Enter).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    wait_daemon_editing_mode(h, want).await
}

#[tokio::test]
async fn test_e2e_editing_mode_live_switch_without_restart() -> E2eResult<()> {
    if !e2e_enabled() {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let mut client = start_daemon(&mut h).await?;
    seed_session(&mut client).await?;
    client
        .update_daemon_config("editing_mode", serde_json::json!("vim"))
        .await
        .map_err(|error| test_error(format!("preset editing_mode: {error}")))?;
    client.disconnect();
    let term = spawn_tui(&h).await?;
    let outcome = async {
        visible(&term, SESSION_TITLE).await?;
        // Vim: the composer is modal.
        typed(&term, "i").await?;
        visible(&term, "INSERT").await?;
        press(&term, Key::Escape).await?;
        visible(&term, "NORMAL").await?;
        // Vim -> Standard in Settings, with the same TUI process.
        open_editing_mode_setting(&term).await?;
        cycle_editing_mode_to(&h, &term, "standard").await?;
        press(&term, Key::Char('q')).await?;
        visible(&term, SESSION_TITLE).await?;
        typed(&term, "i").await?;
        visible(&term, "EDIT").await?;
        typed(&term, "jk: abc").await?;
        visible(&term, "jk: abc").await?;
        // Clear the draft, return to the list, and switch back to Vim.
        press(&term, Key::Ctrl('a')).await?;
        press(&term, Key::Backspace).await?;
        raw(&term, CTRL_H).await?;
        settled(&term).await;
        open_editing_mode_setting(&term).await?;
        cycle_editing_mode_to(&h, &term, "vim").await?;
        press(&term, Key::Char('q')).await?;
        visible(&term, SESSION_TITLE).await?;
        typed(&term, "i").await?;
        visible(&term, "INSERT").await?;
        press(&term, Key::Escape).await?;
        visible(&term, "NORMAL").await?;
        Ok(())
    }
    .await;
    finish(h, &term, outcome).await
}
