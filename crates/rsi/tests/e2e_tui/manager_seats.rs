//! #1627 R5: the seat list in the existing manager console (`:manager
//! workspace`): earlier-session (lineage) rows, the status/halt/archive keys,
//! and the host-project field on the launch form. Fixture data only.
use super::*;
use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("manager seats key: {e:?}")))
}

async fn keys(term: &Terminal, text: &str) -> E2eResult<()> {
    for c in text.chars() {
        press(term, Key::Char(c)).await?;
    }
    Ok(())
}

async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    term.expect(text)
        .timeout(Duration::from_secs(8))
        .await
        .map_err(|e| test_error(format!("expected {text:?}: {e:?}")))
}

async fn session_status(
    client: &mut DaemonClient,
    id: uuid::Uuid,
) -> E2eResult<Option<SessionStatus>> {
    Ok(client
        .list_sessions()
        .await?
        .into_iter()
        .find(|s| s.id == id)
        .map(|s| s.status))
}

async fn wait_status(
    client: &mut DaemonClient,
    id: uuid::Uuid,
    want: &[SessionStatus],
) -> E2eResult<SessionStatus> {
    let mut last = None;
    for _ in 0..200 {
        last = session_status(client, id).await?;
        if let Some(status) = last.filter(|status| want.contains(status)) {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(test_error(format!(
        "session {id} never reached {want:?}; last {last:?}"
    )))
}

async fn launch_seat(
    client: &mut DaemonClient,
    repo: &Path,
    project: uuid::Uuid,
    query: &str,
    title: &str,
) -> E2eResult<uuid::Uuid> {
    client
        .launch_session_with_opts(
            query,
            Some(title),
            Some(repo),
            SessionProvider::Codex,
            Some("gpt-6-astra"),
            None,
            Some(SessionKind::Standard),
            Some(project),
            Some(0),
            Some("low"),
            None,
            None,
            &["e2e".into()],
            None,
            None,
        )
        .await?;
    for _ in 0..200 {
        if let Some(found) = client
            .list_sessions()
            .await?
            .into_iter()
            .find(|s| s.title.as_deref() == Some(title))
        {
            return Ok(found.id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(test_error(format!("session {title} missing")))
}

async fn launch_continuation(
    socket: &Path,
    repo: &Path,
    project: uuid::Uuid,
    prior: uuid::Uuid,
) -> E2eResult<uuid::Uuid> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let (read, mut write) = stream.into_split();
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "LaunchSession",
        "params": {
            "query": "MANAGER_PRESET_E2E_ACTOR",
            "title": "Current seat",
            "working_dir": repo,
            "provider": SessionProvider::Codex,
            "model": "gpt-6-astra",
            "effort": "low",
            "session_kind": SessionKind::Standard,
            "project_id": project,
            "continued_from": prior,
            "max_retries": 0,
            "tags": ["e2e"],
        },
    });
    write.write_all(format!("{frame}\n").as_bytes()).await?;
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
        .await
        .map_err(|_| test_error("LaunchSession reply timed out"))??
    {
        let reply: serde_json::Value = serde_json::from_str(&line)?;
        if reply["id"] != 1 {
            continue;
        }
        if let Some(error) = reply.get("error") {
            return Err(test_error(format!("LaunchSession refused: {error}")));
        }
        let id = reply["result"]
            .as_str()
            .or_else(|| reply["result"]["session_id"].as_str())
            .ok_or_else(|| test_error(format!("LaunchSession result shape: {reply}")))?;
        return Ok(id.parse()?);
    }
    Err(test_error("daemon closed before the LaunchSession reply"))
}

/// One global seat that rotated once: the earlier session sits behind the
/// current one in the seat list, and the operator can read its status, is
/// refused talking to it, archives it, and halts the current seat, all from
/// the console. The launch form names its host project.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_e2e_manager_seat_list_lineage_keys_and_host() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let socket_dir = tempfile::Builder::new()
        .prefix("rsi-seat-")
        .tempdir_in("/tmp")?;
    h.socket_path = socket_dir.path().join("daemon.sock");
    let repo = h.home_dir.join("manager-seats-repo");
    fs::create_dir(&repo)?;
    for args in [
        vec!["init", "-b", "rolling"],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.invalid"],
    ] {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()?;
        if !out.status.success() {
            return Err(test_error(String::from_utf8_lossy(&out.stderr).to_string()));
        }
    }
    fs::write(repo.join("fixture.txt"), "manager seats fixture\n")?;
    for args in [vec!["add", "fixture.txt"], vec!["commit", "-m", "fixture"]] {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()?;
        if !out.status.success() {
            return Err(test_error("fixture git failed"));
        }
    }
    fs::write(
        h.bin_dir.join("codex"),
        include_bytes!("../fixtures/manager-preset-actor.py"),
    )?;
    fs::set_permissions(h.bin_dir.join("codex"), fs::Permissions::from_mode(0o755))?;
    h.phase = "manager-seats-daemon";
    h.start_daemon()?;
    let mut client = h.wait_for_daemon().await?;
    let project = client
        .create_project("Seat fixture project", Some(&repo), None, None)
        .await?;
    // The earlier seat session ends at once (whatever its exit status: it is
    // history either way); the current one stays live.
    let prior = launch_seat(&mut client, &repo, project.id, "prior seat", "Prior seat").await?;
    wait_status(
        &mut client,
        prior,
        &[
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
        ],
    )
    .await?;
    // The current seat continues the earlier one, as a context rotation
    // records it (`continued_from`). The client has no field for it, so this
    // sends the LaunchSession frame itself.
    let current = launch_continuation(&h.socket_path, &repo, project.id, prior).await?;
    wait_status(&mut client, current, &[SessionStatus::Running]).await?;
    client
        .configure_global_manager(ConfigureGlobalManagerRequestV1 {
            session_id: current,
            project_ids: vec![project.id],
            allowed_launches: vec![ManagerLaunchChoiceV2 {
                provider: SessionProvider::Codex,
                model: "gpt-6-astra".into(),
                effort: Some("low".into()),
            }],
            project_policy: ManagerPolicyV2::default(),
            expected_grant_version: 0,
            idempotency_key: "seats-e2e-grant".into(),
        })
        .await?;
    let prior_short = prior.to_string()[..8].to_string();

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
        .map_err(|e| test_error(format!("manager seats TUI: {e:?}")))?;
    let outcome = async {
        visible(&term, "2 sessions").await?;
        keys(&term, ":manager workspace").await?;
        press(&term, Key::Enter).await?;
        // The lineage row sits under the seat, with the same host line.
        visible(&term, &format!("↳ prior {prior_short}")).await?;
        visible(&term, "Host project Seat fixture project").await?;
        // Status of the current seat, then of the earlier session.
        keys(&term, "s").await?;
        visible(&term, "seat ACTIVE").await?;
        keys(&term, "j").await?;
        keys(&term, "s").await?;
        visible(&term, "earlier session, read only").await?;
        visible(&term, "status ").await?;
        // Earlier sessions are read-only history.
        keys(&term, "i").await?;
        visible(&term, "read-only history").await?;
        // Archive is refused on the current seat and works on the earlier one.
        keys(&term, "k").await?;
        keys(&term, "a").await?;
        visible(&term, "is the current seat").await?;
        keys(&term, "j").await?;
        keys(&term, "a").await?;
        // The session list leaves archived sessions out.
        let mut archived = false;
        for _ in 0..200 {
            if matches!(
                session_status(&mut client, prior).await?,
                None | Some(SessionStatus::Archived)
            ) {
                archived = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !archived {
            return Err(test_error("the earlier seat session was not archived"));
        }
        // The launch form names a host project, defaulting to a covered one.
        keys(&term, "n").await?;
        visible(&term, "Host").await?;
        visible(&term, "Seat fixture project (default)").await?;
        press(&term, Key::Escape).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Halt the current seat from the console: refresh, select it, halt.
        keys(&term, "r").await?;
        keys(&term, "kk").await?;
        keys(&term, "x").await?;
        wait_status(
            &mut client,
            current,
            &[SessionStatus::Interrupted, SessionStatus::Completed],
        )
        .await?;
        // Open the seat: Enter leaves the console on the seat's own session.
        keys(&term, ":manager workspace").await?;
        press(&term, Key::Enter).await?;
        visible(&term, "Global manager workspace").await?;
        press(&term, Key::Enter).await?;
        visible(&term, "Current seat").await?;
        visible(&term, "MANAGER_PRESET_E2E_ACTOR").await?;
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;
    h.final_screen_text = Some(term.screen().await.text());
    let _ = term.send_key(Key::Ctrl('c')).await;
    let _ = client.interrupt_session(current).await;
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; manager seats artifacts: {}",
            artifacts.display()
        )));
    }
    Ok(())
}
