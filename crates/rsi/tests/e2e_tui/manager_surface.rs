use super::*;
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("manager surface key: {e:?}")))
}
async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    term.expect(text)
        .timeout(Duration::from_secs(8))
        .await
        .map_err(|e| test_error(format!("expected {text:?}: {e:?}")))
}
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_e2e_manager_surface_sections_share_one_entry() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let socket_dir = tempfile::Builder::new()
        .prefix("rsi-ms-")
        .tempdir_in("/tmp")?;
    h.socket_path = socket_dir.path().join("daemon.sock");
    let repo = h.home_dir.join("manager-surface-repo");
    fs::create_dir(&repo)?;
    for args in [
        ["init", "-b", "rolling"],
        ["config", "user.name", "Fixture"],
        ["config", "user.email", "fixture@example.invalid"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(test_error(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
    }
    fs::write(repo.join("fixture.txt"), "manager surface fixture\n")?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["add", "fixture.txt"])
        .output()?;
    if !output.status.success() {
        return Err(test_error("git add failed"));
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["commit", "-m", "fixture"])
        .output()?;
    if !output.status.success() {
        return Err(test_error("git commit failed"));
    }
    fs::write(
        h.bin_dir.join("codex"),
        include_bytes!("../fixtures/manager-preset-actor.py"),
    )?;
    fs::set_permissions(h.bin_dir.join("codex"), fs::Permissions::from_mode(0o755))?;
    h.phase = "manager-surface-daemon";
    h.start_daemon()?;
    let mut client = match h.wait_for_daemon().await {
        Ok(client) => client,
        Err(error) => {
            h.shutdown_daemon();
            let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
            return Err(test_error(format!(
                "{error}; manager surface artifacts: {}",
                artifacts.display()
            )));
        }
    };
    let project = client
        .create_project("Manager surface fixture", Some(&repo), None, None)
        .await?;
    let group = client
        .create_container(
            SessionKind::Group,
            "Surface Group",
            None,
            Some(project.id),
            &["e2e".into()],
            None,
        )
        .await?;
    let epic = client
        .create_container(
            SessionKind::Epic,
            "Surface Epic",
            Some(group),
            Some(project.id),
            &["e2e".into()],
            None,
        )
        .await?;
    client
        .launch_session_with_opts(
            "MANAGER_PRESET_E2E_ACTOR",
            Some("Surface manager"),
            Some(&repo),
            SessionProvider::Codex,
            Some("gpt-6-astra"),
            None,
            Some(SessionKind::Standard),
            Some(project.id),
            Some(0),
            Some("low"),
            None,
            None,
            &["e2e".into()],
            None,
            None,
        )
        .await?;
    let mut manager = None;
    for _ in 0..200 {
        manager = client
            .list_sessions()
            .await?
            .into_iter()
            .find(|s| s.title.as_deref() == Some("Surface manager"));
        if manager
            .as_ref()
            .is_some_and(|s| s.status == SessionStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let manager = manager.ok_or_else(|| test_error("surface manager session missing"))?;
    client
        .configure_harness_manager(ConfigureHarnessManagerRequestV1 {
            project_id: project.id,
            session_id: manager.id,
            epic_ids: Some(vec![epic]),
            group_ids: vec![],
            expected_row_version: 0,
        })
        .await?;
    let term = Terminal::builder()
        .size(150, 46)
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
        .map_err(|e| test_error(format!("manager TUI: {e:?}")))?;
    let outcome = async {
        visible(&term, "Surface manager").await?;
        press(&term, Key::Char(' ')).await?;
        press(&term, Key::Char('g')).await?;
        press(&term, Key::Char('b')).await?;
        visible(&term, "[1 Board]").await?;
        press(&term, Key::Char('2')).await?;
        visible(&term, "[2 Decisions]").await?;
        press(&term, Key::Char('3')).await?;
        visible(&term, "[3 Inbox]").await?;
        press(&term, Key::Char('5')).await?;
        visible(&term, "[5 Policy]").await?;
        visible(&term, "Active session limit").await?;
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;
    h.final_screen_text = Some(term.screen().await.text());
    let _ = term.send_key(Key::Ctrl('c')).await;
    if manager.status == SessionStatus::Running {
        let _ = client.interrupt_session(manager.id).await;
    }
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; manager surface artifacts: {}",
            artifacts.display()
        )));
    }
    outcome
}
