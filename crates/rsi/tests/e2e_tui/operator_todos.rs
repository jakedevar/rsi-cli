//! Operator to-do panel (#1747): items added through the real operator RPC,
//! the panel opened in session detail, toggled with `<Space>dt` (the choice
//! is remembered in `state.json`), yanked with `<Space>dy` (vim `y`: copies,
//! closes nothing) and marked done with `<Space>dd` after a y/n prompt, which
//! closes the item through the real `OperatorTodoClose` RPC; `<Space>du` then
//! undoes the close through `OperatorTodoReopen` (#2950).
use super::*;
use rsi_common::operator_todos::{
    OperatorTodoAddRequestV1, OperatorTodoAddResultV1, OperatorTodoStatusV1,
};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("operator todo key: {e:?}")))
}

async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    term.expect(text)
        .timeout(Duration::from_secs(15))
        .await
        .map_err(|e| test_error(format!("expected {text:?}: {e:?}")))
}

/// Open the session detail from the list and wait for the to-do panel title.
///
/// The first session list (and its "2 todo" badge) can render before the TUI
/// handles keys, so a single `Enter` is sometimes dropped and nothing retries
/// it. The condition the test cares about is "the panel title is on screen",
/// so press `Enter` again until it is, within one generous deadline (#2839).
/// A second `Enter` inside an already open detail view focuses the composer,
/// which swallows the later `<Space>d` chords, so it is only sent while the
/// detail view (its first conversation event) is not on screen.
async fn open_detail_with_panel(term: &Terminal, title: &str) -> E2eResult<()> {
    const DETAIL_MARKER: &str = "\u{258e} #1";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if !term.screen().await.text().contains(DETAIL_MARKER) {
            press(term, Key::Enter).await?;
        }
        if term
            .expect(title)
            .timeout(Duration::from_secs(4))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let screen = term.screen().await.text();
            return Err(test_error(format!(
                "to-do panel never showed {title:?}; screen:\n{screen}"
            )));
        }
    }
}

/// `<Space>d<key>`: the leader chord of the to-do family.
async fn todo_chord(term: &Terminal, key: char) -> E2eResult<()> {
    press(term, Key::Char(' ')).await?;
    press(term, Key::Char('d')).await?;
    press(term, Key::Char(key)).await
}

/// One operator RPC over the daemon socket (the client keeps `request`
/// private, and `OperatorTodoAdd` has no TUI caller).
fn operator_add(socket: &Path, request: &OperatorTodoAddRequestV1) -> E2eResult<()> {
    let mut stream = UnixStream::connect(socket)?;
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "OperatorTodoAdd",
        "params": request,
    });
    stream.write_all(format!("{frame}\n").as_bytes())?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let response: serde_json::Value = serde_json::from_str(line.trim())?;
    if let Some(error) = response.get("error") {
        return Err(test_error(format!("OperatorTodoAdd refused: {error}")));
    }
    let added: OperatorTodoAddResultV1 = serde_json::from_value(response["result"].clone())?;
    if added.todo.title != request.title {
        return Err(test_error("OperatorTodoAdd echoed another item"));
    }
    Ok(())
}

fn add_request(title: &str, how: &str, priority: u8) -> OperatorTodoAddRequestV1 {
    OperatorTodoAddRequestV1 {
        title: title.into(),
        how: how.into(),
        link: None,
        priority: Some(priority),
        project_id: None,
        idempotency_key: format!("e2e-{priority}"),
    }
}

fn panel_hidden_on_disk(home: &Path) -> E2eResult<bool> {
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".rsi/state.json"))?)?;
    Ok(state["todo_panel_hidden"].as_bool().unwrap_or(false))
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_e2e_operator_todo_panel_opens_toggles_and_marks_done() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let socket_dir = tempfile::Builder::new()
        .prefix("rsi-td-")
        .tempdir_in("/tmp")?;
    h.socket_path = socket_dir.path().join("daemon.sock");
    let repo = h.home_dir.join("todo-panel-repo");
    fs::create_dir(&repo)?;
    fs::write(
        h.bin_dir.join("codex"),
        include_bytes!("../fixtures/manager-preset-actor.py"),
    )?;
    fs::set_permissions(h.bin_dir.join("codex"), fs::Permissions::from_mode(0o755))?;
    h.phase = "operator-todo-daemon";
    h.start_daemon()?;
    let mut client = match h.wait_for_daemon().await {
        Ok(client) => client,
        Err(error) => {
            h.shutdown_daemon();
            let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
            return Err(test_error(format!(
                "{error}; operator todo artifacts: {}",
                artifacts.display()
            )));
        }
    };
    let seeded: E2eResult<_> = async {
        // An unset `editing_mode` makes the TUI decide at startup between a
        // first-start Standard/Vim prompt and silent Vim, depending on whether
        // the first session snapshot has landed. When the prompt wins, the
        // `Enter` meant for the session list answers it (choosing Standard),
        // and the later `<Space>d` chords are typed into a Standard composer.
        // The chords are Vim-mode normal bindings, so pin the mode (#2839).
        client
            .update_daemon_config("editing_mode", serde_json::json!("vim"))
            .await?;
        client
            .launch_session_with_opts(
                "MANAGER_PRESET_E2E_ACTOR",
                Some("Todo panel session"),
                Some(&repo),
                SessionProvider::Codex,
                Some("gpt-6-astra"),
                None,
                Some(SessionKind::Standard),
                None,
                Some(0),
                Some("low"),
                None,
                None,
                &["e2e".into()],
                None,
                None,
            )
            .await?;
        operator_add(
            &h.socket_path,
            &add_request("Log in to Codex on the laptop", "codex login", 1),
        )?;
        operator_add(
            &h.socket_path,
            &add_request("Turn on phone answers", "rsi phone on", 3),
        )?;
        Ok(())
    }
    .await;
    if let Err(error) = seeded {
        h.shutdown_daemon();
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; operator todo artifacts: {}",
            artifacts.display()
        )));
    }
    let term = Terminal::builder()
        .size(200, 50)
        .env("HOME", &h.path_string(&h.home_dir)?)
        .env("RSI_DAEMON_SOCKET_PATH", &h.path_string(&h.socket_path)?)
        .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
        .env("RSI_SESSION_TOKEN", "")
        .env("PATH", &h.isolated_path)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("LANG", "C.UTF-8")
        .env("TZ", "UTC")
        .spawn(&h.rsi_bin, &[])
        .await
        .map_err(|e| test_error(format!("operator todo TUI: {e:?}")))?;
    let home = h.home_dir.clone();
    let outcome = async {
        // The badge is on every screen, including the session list.
        visible(&term, "2 todo").await?;
        visible(&term, "Todo panel session").await?;
        // Open the session detail: the panel sits to its right.
        open_detail_with_panel(&term, "To-do · 2 open").await?;
        visible(&term, "codex login").await?;
        visible(&term, "rsi phone on").await?;

        // Toggle off: the choice is written to state.json and the badge stays.
        todo_chord(&term, 't').await?;
        let mut hidden = false;
        for _ in 0..40 {
            if panel_hidden_on_disk(&home).unwrap_or(false) {
                hidden = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !hidden {
            return Err(test_error("hiding the panel was not remembered"));
        }
        visible(&term, "2 todo").await?;
        // Toggle back on.
        todo_chord(&term, 't').await?;
        visible(&term, "To-do · 2 open").await?;
        if panel_hidden_on_disk(&home)? {
            return Err(test_error("showing the panel was not remembered"));
        }

        // `y` is vim's yank: it copies the command and closes nothing.
        todo_chord(&term, 'y').await?;
        visible(&term, "Copied to-do command: codex login").await?;
        visible(&term, "To-do · 2 open").await?;
        if client.list_operator_todos(None).await?.open.len() != 2 {
            return Err(test_error("yanking an item closed it"));
        }

        // Done asks first: declining the prompt leaves both items open.
        todo_chord(&term, 'd').await?;
        visible(&term, "y/Enter yes").await?;
        press(&term, Key::Char('n')).await?;
        press(&term, Key::Escape).await?;
        visible(&term, "To-do · 2 open").await?;
        if client.list_operator_todos(None).await?.open.len() != 2 {
            return Err(test_error("declining the prompt closed an item"));
        }

        // Mark the selected (most urgent) item done after confirming.
        todo_chord(&term, 'd').await?;
        visible(&term, "y/Enter yes").await?;
        press(&term, Key::Char('y')).await?;
        visible(&term, "To-do · 1 open").await?;
        visible(&term, "1 todo").await?;
        let mut done = false;
        for _ in 0..40 {
            let list = client.list_operator_todos(None).await?;
            if list.open.len() == 1 && list.open[0].title == "Turn on phone answers" {
                done = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !done {
            return Err(test_error("the daemon still lists the finished item open"));
        }
        let closed = client
            .list_operator_todos(Some(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH))
            .await?;
        let finished = closed
            .recently_closed
            .iter()
            .find(|item| item.title == "Log in to Codex on the laptop")
            .ok_or_else(|| test_error("the finished item is missing from the closed list"))?;
        if finished.status != OperatorTodoStatusV1::Done {
            return Err(test_error(format!(
                "finished item status is {:?}",
                finished.status
            )));
        }

        // Undo: `<Space>du` reopens it through `OperatorTodoReopen`.
        todo_chord(&term, 'u').await?;
        visible(&term, "To-do · 2 open").await?;
        let restored = client.list_operator_todos(None).await?;
        if restored.open.len() != 2
            || !restored
                .open
                .iter()
                .any(|item| item.title == "Log in to Codex on the laptop")
        {
            return Err(test_error("the undo did not bring the item back"));
        }
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;
    h.final_screen_text = Some(term.screen().await.text());
    let _ = quit_tui(&term).await;
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; operator todo artifacts: {}",
            artifacts.display()
        )));
    }
    outcome
}
