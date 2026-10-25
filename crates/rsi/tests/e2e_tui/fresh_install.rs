//! #1626 criterion 1: README "Start here" prompt (a), walked on a FRESH
//! install. An empty HOME, an empty daemon, and a scripted Claude print-mode
//! fixture (`claude-fresh-install.py`, no inference): first start, the
//! Standard/Vim prompt, adding the repo as a project, a root session,
//! `:manager appoint`, the Execute policy preset, and (prompt b) a global seat
//! from a second root session. Fixture data only.
use super::*;
use rsi_common::harness_manager_v2::ManagerOperatingModeV2;

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("fresh install key: {e:?}")))
}

async fn keys(term: &Terminal, text: &str) -> E2eResult<()> {
    for c in text.chars() {
        press(term, Key::Char(c)).await?;
    }
    Ok(())
}

/// Type a `:` command and run it, as the README tells the operator to.
async fn command(term: &Terminal, text: &str) -> E2eResult<()> {
    keys(term, &format!(":{text}")).await?;
    press(term, Key::Enter).await
}

async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    term.expect(text)
        .timeout(Duration::from_secs(10))
        .await
        .map_err(|e| test_error(format!("expected {text:?}: {e:?}")))
}

async fn wait_session(
    client: &mut DaemonClient,
    query: &str,
    want: SessionStatus,
) -> E2eResult<uuid::Uuid> {
    let mut last = None;
    for _ in 0..200 {
        if let Some(found) = client
            .list_sessions()
            .await?
            .into_iter()
            .find(|s| s.query.contains(query))
        {
            if found.status == want {
                return Ok(found.id);
            }
            last = Some(found.status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(test_error(format!(
        "session {query:?} never reached {want:?}; last {last:?}"
    )))
}

/// The whole walk takes about 15 s; this bounds a wedged one (#3431).
const SCENARIO_BUDGET: Duration = Duration::from_secs(150);

/// README "Seat a project manager on the current repo", then "Seat a global
/// manager over several projects", from first start with nothing configured.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_e2e_fresh_install_appoint_project_manager() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let socket_dir = tempfile::Builder::new()
        .prefix("rsi-fresh-")
        .tempdir_in("/tmp")?;
    h.socket_path = socket_dir.path().join("daemon.sock");
    // The "current repo": the operator would run `rsi` inside it.
    let repo = h.home_dir.join("starter-repo");
    fs::create_dir(&repo)?;
    fs::write(
        h.bin_dir.join("claude"),
        include_bytes!("../fixtures/claude-fresh-install.py"),
    )?;
    fs::set_permissions(h.bin_dir.join("claude"), fs::Permissions::from_mode(0o755))?;
    h.phase = "fresh-install-daemon";
    h.start_daemon_with_fixture_scopes()?;
    // Nothing is seeded: no operator project, no session, no editing mode. The
    // daemon registers exactly one built-in home project on start (hub #3315).
    let mut client = h.wait_for_daemon().await?;
    let projects = client.list_projects().await?;
    assert_eq!(
        projects.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![rsi_common::Project::HOME_ID]
    );
    assert!(client.list_sessions().await?.is_empty());

    let term = Terminal::builder()
        .size(160, 50)
        .env("HOME", &h.path_string(&h.home_dir)?)
        .env("RSI_DAEMON_SOCKET_PATH", &h.path_string(&h.socket_path)?)
        .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
        .env("RSI_SESSION_TOKEN", "")
        .env("PATH", &h.isolated_path)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("LANG", "C.UTF-8")
        .spawn(&h.rsi_bin, &[])
        .await
        .map_err(|e| test_error(format!("fresh install TUI: {e:?}")))?;
    // A wedged daemon or TUI must fail this test with preserved artifacts, never
    // hang it until the gate's 20-minute guard kills the whole shard unparsed
    // (#3431: the gate saw a TimedOut with no output at all).
    let outcome = tokio::time::timeout(SCENARIO_BUDGET, async {
        // First start: the required Standard/Vim prompt comes before anything.
        visible(&term, "Choose how you edit text").await?;
        keys(&term, "s").await?;
        visible(&term, "No sessions").await?;
        // README step 1: `<Space>p`, add the repo as a project (`^n` form).
        keys(&term, " p").await?;
        visible(&term, "Workspaces").await?;
        press(&term, Key::Ctrl('n')).await?;
        visible(&term, "New Project").await?;
        keys(&term, "Starter repo").await?;
        press(&term, Key::Tab).await?;
        keys(&term, &h.path_string(&repo)?).await?;
        press(&term, Key::Enter).await?;
        // The picker reloads and lists the project just saved; pick it.
        visible(&term, "Starter repo  ~/starter-repo").await?;
        keys(&term, "Starter").await?;
        press(&term, Key::Enter).await?;
        visible(&term, "Starter repo / Sessions").await?;
        let project = client
            .list_projects()
            .await?
            .into_iter()
            .find(|p| p.name == "Starter repo")
            .ok_or_else(|| test_error("the project was not created"))?;
        // README step 2: `:blank <text>` starts a Standard root session.
        command(&term, "blank Coordinate this project's work").await?;
        let seat = wait_session(
            &mut client,
            "Coordinate this project's work",
            SessionStatus::Completed,
        )
        .await?;
        visible(&term, "scripted fresh-install fixture ready").await?;
        // README step 3: `:manager appoint`, default scope, Enter.
        command(&term, "manager appoint").await?;
        visible(&term, "Appoint Harness Manager").await?;
        visible(&term, "Whole project").await?;
        press(&term, Key::Enter).await?;
        visible(&term, "Manager scope saved: whole project").await?;
        let manager = client
            .get_harness_manager(project.id)
            .await?
            .ok_or_else(|| test_error("no manager was seated"))?;
        assert_eq!(manager.manager_session_id, seat);
        // README step 4: `:manager policy`, Execute (j, Enter), `s` saves.
        command(&term, "manager policy").await?;
        visible(&term, "Saved: no policy").await?;
        keys(&term, "j").await?;
        press(&term, Key::Enter).await?;
        visible(&term, "Draft: Execute").await?;
        keys(&term, "s").await?;
        visible(&term, "Saved: Execute").await?;
        let policy = client
            .get_harness_manager_policy(project.id)
            .await?
            .ok_or_else(|| test_error("the Execute policy was not saved"))?;
        assert_eq!(policy.policy.mode, ManagerOperatingModeV2::Execute);
        assert_eq!(policy.manager_session_id, seat);
        press(&term, Key::Escape).await?;
        // Let the console close before the next `:` command.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Prompt (b): a second Standard root session becomes the global seat.
        command(&term, "blank Coordinate every project").await?;
        let global_seat = wait_session(
            &mut client,
            "Coordinate every project",
            SessionStatus::Completed,
        )
        .await?;
        // The seat is the SELECTED session, and a new session does not take the
        // selection: move onto it (`j`) before appointing.
        keys(&term, "j").await?;
        command(&term, "manager global appoint Starter repo").await?;
        let mut seated = None;
        for _ in 0..100 {
            seated = client.get_global_manager().await?;
            if seated.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let grant = seated.ok_or_else(|| test_error("no global manager was seated"))?;
        assert_eq!(grant.state, "active");
        assert_eq!(grant.seat_session_id, global_seat);
        assert_eq!(grant.project_ids, vec![project.id]);
        // `:manager global` shows the seated grant. This runs BEFORE the starter
        // prompt on purpose: its answer is a transient toast, and once the project
        // manager ends that turn with no wake the daemon's idle backstop re-fires
        // it many times a second, each firing posting a "Scheduled: ... fired"
        // toast that replaces this one before the poll can read it.
        command(&term, "manager global").await?;
        visible(&term, "Global manager v1 seat").await?;

        // README step 5: back on the manager's row (`k`), Enter opens it, the
        // text box is already live in Standard mode; type a starter prompt and
        // Enter sends it.
        keys(&term, "k").await?;
        press(&term, Key::Enter).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        keys(&term, "Triage the open Issues").await?;
        press(&term, Key::Enter).await?;
        // The manager answers the sent prompt: transcript event #4 is the
        // scripted reply to the starter prompt (#3).
        visible(&term, "#4").await?;
        // Standard editing: the text box owns every key, so `Ctrl-H` hands
        // focus back to the list (the kitty-protocol form; a legacy terminal
        // sends Backspace) before the next `:` command.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        term.send_raw(b"\x1b[104;5u")
            .await
            .map_err(|e| test_error(format!("ctrl-h: {e:?}")))?;
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The editing-mode choice was stored by the daemon.
        let config = client.get_daemon_config().await?;
        assert_eq!(config["editing_mode"], "standard");
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    })
    .await
    .unwrap_or_else(|_| {
        Err(test_error(format!(
            "fresh install scenario exceeded {SCENARIO_BUDGET:?}"
        )))
    });
    h.final_screen_text = Some(
        tokio::time::timeout(Duration::from_secs(10), term.screen())
            .await
            .map_or_else(|_| "screen read timed out".to_owned(), |s| s.text()),
    );
    let _ = tokio::time::timeout(Duration::from_secs(30), quit_tui(&term)).await;
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; fresh install artifacts: {}",
            artifacts.display()
        )));
    }
    Ok(())
}
