//! #1626 slice 3: an agent-created project appears in the TUI and the TUI
//! follows it, unless the operator turned `follow_agent_created_projects` off.
//! The creator is a scripted Codex fixture (no model inference) that calls
//! `AgentCreateProject` with its own daemon-minted token. Fixture data only.
use super::*;
use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
use rsi_common::harness_manager_v2::{
    ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerPolicyV2,
};
use serde_json::{Value, json};

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("agent projects key: {e:?}")))
}

async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    term.expect(text)
        .timeout(Duration::from_secs(10))
        .await
        .map_err(|e| test_error(format!("expected {text:?}: {e:?}")))
}

fn git(root: &Path, args: &[&str]) -> E2eResult<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(test_error(format!(
            "fixture git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// The project-tab dots at the right end of the top bar: `\u{25CF}` per tab
/// and `\u{25C9}` for the active one, left to right. The running-sessions dot
/// at the left of the bar is not one of them.
fn tab_dots(screen: &str) -> String {
    let mut dots: Vec<char> = screen
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end()
        .chars()
        .rev()
        .take_while(|c| matches!(c, '\u{25CF}' | '\u{25C9}' | ' '))
        .filter(|c| *c != ' ')
        .collect();
    dots.reverse();
    dots.into_iter().collect()
}

/// The tab dots once they have not changed for a second (startup restores and
/// opens project tabs asynchronously).
async fn settled_tab_dots(term: &Terminal) -> String {
    let mut last = tab_dots(&term.screen().await.text());
    let mut stable_since = Instant::now();
    while stable_since.elapsed() < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = tab_dots(&term.screen().await.text());
        if now != last {
            last = now;
            stable_since = Instant::now();
        }
    }
    last
}

async fn wait_tab_dots(term: &Terminal, expected: &str) -> E2eResult<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let dots = tab_dots(&term.screen().await.text());
        if dots == expected {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(test_error(format!(
                "project tab dots {dots:?}, wanted {expected:?}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn spawn_tui(h: &E2eHarness) -> E2eResult<impl std::future::Future<Output = E2eResult<Terminal>>> {
    let home = h.path_string(&h.home_dir)?;
    let socket = h.path_string(&h.socket_path)?;
    let path = h.isolated_path.clone();
    let bin = h.rsi_bin.clone();
    Ok(async move {
        Terminal::builder()
            .size(150, 46)
            .env("HOME", &home)
            .env("RSI_DAEMON_SOCKET_PATH", &socket)
            .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
            .env("RSI_SESSION_TOKEN", "")
            .env("PATH", &path)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .spawn(&bin, &[])
            .await
            .map_err(|e| test_error(format!("agent projects TUI: {e:?}")))
    })
}

/// Hand `AgentCreateProject` to the fixture agent and return the daemon reply.
async fn agent_create(h: &E2eHarness, n: usize, name: &str, path: &Path) -> E2eResult<Value> {
    let request = h.bin_dir.join(format!("agent-request-{n}.json.tmp"));
    fs::write(
        &request,
        serde_json::to_vec(&json!({
            "method": "AgentCreateProject",
            "params": {"name": name, "path": path.to_string_lossy()}
        }))?,
    )?;
    fs::rename(&request, h.bin_dir.join(format!("agent-request-{n}.json")))?;
    let result = h.bin_dir.join(format!("agent-result-{n}.json"));
    for _ in 0..300 {
        if result.exists() {
            let reply: Value = serde_json::from_slice(&fs::read(&result)?)?;
            return Ok(reply);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(test_error(
        "agent fixture never answered AgentCreateProject",
    ))
}

fn created_id(reply: &Value) -> E2eResult<uuid::Uuid> {
    let created = reply
        .get("result")
        .ok_or_else(|| test_error(format!("AgentCreateProject refused: {reply}")))?;
    created
        .get("project_id")
        .and_then(Value::as_str)
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .ok_or_else(|| test_error(format!("no project id in {created}")))
}

fn made_dir(h: &E2eHarness, name: &str) -> E2eResult<PathBuf> {
    let dir = h.home_dir.join(name);
    fs::create_dir(&dir)?;
    git(&dir, &["init", "-b", "rolling"])?;
    Ok(dir)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_e2e_agent_created_project_is_followed_unless_the_setting_is_off() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let socket_dir = tempfile::Builder::new()
        .prefix("rsi-ap-")
        .tempdir_in("/tmp")?;
    h.socket_path = socket_dir.path().join("daemon.sock");
    let repo = h.home_dir.join("agent-projects-repo");
    fs::create_dir(&repo)?;
    git(&repo, &["init", "-b", "rolling"])?;
    git(&repo, &["config", "user.name", "Fixture"])?;
    git(&repo, &["config", "user.email", "fixture@example.invalid"])?;
    fs::write(repo.join("fixture.txt"), "agent projects fixture\n")?;
    git(&repo, &["add", "fixture.txt"])?;
    git(&repo, &["commit", "-m", "fixture"])?;
    let first_dir = made_dir(&h, "agent-made-first")?;
    let second_dir = made_dir(&h, "agent-made-second")?;
    fs::write(
        h.bin_dir.join("codex"),
        include_bytes!("../fixtures/agent-request-actor.py"),
    )?;
    fs::set_permissions(h.bin_dir.join("codex"), fs::Permissions::from_mode(0o755))?;
    h.phase = "agent-projects-daemon";
    h.start_daemon()?;
    let mut client = match h.wait_for_daemon().await {
        Ok(client) => client,
        Err(error) => {
            h.shutdown_daemon();
            let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
            return Err(test_error(format!(
                "{error}; agent projects artifacts: {}",
                artifacts.display()
            )));
        }
    };
    let project = client
        .create_project("Agent follow fixture", Some(&repo), None, None)
        .await?;
    client
        .launch_session_with_opts(
            "AGENT_PROJECT_E2E_ACTOR",
            Some("Agent project creator"),
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
            .find(|s| s.title.as_deref() == Some("Agent project creator"));
        if manager
            .as_ref()
            .is_some_and(|s| s.status == SessionStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let manager = manager.ok_or_else(|| test_error("agent project creator missing"))?;
    // The creator is the global manager seat: Execute mode over the fixture
    // project, so it may create projects and the TUI can offer its grant.
    let seat = ConfigureGlobalManagerRequestV1 {
        session_id: manager.id,
        project_ids: vec![project.id],
        allowed_launches: vec![ManagerLaunchChoiceV2 {
            provider: SessionProvider::Codex,
            model: "gpt-6-astra".into(),
            effort: Some("low".into()),
        }],
        project_policy: ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Execute,
            ..ManagerPolicyV2::default()
        },
        expected_grant_version: 0,
        idempotency_key: "agent-projects-seat".into(),
    };
    let grant = client.configure_global_manager(seat).await?;
    let mut term = spawn_tui(&h)?.await?;
    let outcome = async {
        visible(&term, "Agent project creator").await?;
        let before = settled_tab_dots(&term).await;

        // 1. Follow on (the default): the new project's tab opens and is active.
        h.phase = "agent-projects-follow-on";
        let reply = agent_create(&h, 0, "Agent made first", &first_dir).await?;
        let first_id = created_id(&reply)?;
        assert!(
            client
                .list_projects()
                .await?
                .iter()
                .any(|p| p.id == first_id && p.name == "Agent made first"),
            "daemon lists the agent-created project"
        );
        // One tab shows no dots; the new project's tab joins it and is active.
        let others = if before.is_empty() {
            "\u{25CF}".to_string()
        } else {
            before.replace('\u{25C9}', "\u{25CF}")
        };
        let followed = format!("{others}\u{25C9}");
        wait_tab_dots(&term, &followed).await?;
        // The one-key grant offer: the creator is the active seat and the new
        // project is not in its grant. `y` adds it; the agent never does.
        visible(
            &term,
            "Add \"Agent made first\" to the global manager's grant?",
        )
        .await?;
        fs::write(
            h.artifacts_dir.join("offer.txt"),
            term.screen().await.text(),
        )?;
        press(&term, Key::Char('y')).await?;
        let mut granted = None;
        for _ in 0..100 {
            let current = client.get_global_manager().await?;
            if current
                .as_ref()
                .is_some_and(|g| g.project_ids.contains(&first_id))
            {
                granted = current;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let granted = granted.ok_or_else(|| test_error("the grant never gained the project"))?;
        assert_eq!(granted.grant_version, grant.grant_version + 1);
        assert_eq!(granted.project_ids, vec![project.id, first_id]);
        fs::write(
            h.artifacts_dir.join("followed.txt"),
            term.screen().await.text(),
        )?;

        // 2. Follow off: the project still appears, the view does not move.
        h.phase = "agent-projects-follow-off";
        let _ = term.send_key(Key::Ctrl('c')).await;
        client
            .update_daemon_config("follow_agent_created_projects", json!(false))
            .await?;
        let term_off = spawn_tui(&h)?.await?;
        let off = async {
            // A restart restores the project tabs, so wait for them rather
            // than for a session row (the restored tab may be a project's).
            let restored = Instant::now() + Duration::from_secs(10);
            while tab_dots(&term_off.screen().await.text()).is_empty() {
                if Instant::now() > restored {
                    return Err(test_error(
                        "the restarted TUI never showed its project tabs",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let steady = settled_tab_dots(&term_off).await;
            let reply = agent_create(&h, 1, "Agent made second", &second_dir).await?;
            let second_id = created_id(&reply)?;
            visible(&term_off, "Agent created project").await?;
            let deadline = Instant::now() + Duration::from_secs(10);
            // The project is registered and listed ...
            loop {
                if client
                    .list_projects()
                    .await?
                    .iter()
                    .any(|p| p.id == second_id)
                {
                    break;
                }
                if Instant::now() > deadline {
                    return Err(test_error("second project never listed"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            // ... and the TUI stays on the tab it was on.
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(tab_dots(&term_off.screen().await.text()), steady);
            fs::write(
                h.artifacts_dir.join("not-followed.txt"),
                term_off.screen().await.text(),
            )?;
            // Following off leaves the grant untouched until the operator acts.
            let unchanged = client
                .get_global_manager()
                .await?
                .ok_or_else(|| test_error("grant"))?;
            assert_eq!(unchanged.grant_version, granted.grant_version);
            Ok::<(), Box<dyn Error + Send + Sync>>(())
        }
        .await;
        let _ = term_off.send_key(Key::Ctrl('c')).await;
        off
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
        for name in ["offer.txt", "followed.txt", "not-followed.txt"] {
            let source = h.artifacts_dir.join(name);
            if source.exists() {
                fs::copy(source, artifacts.join(name))?;
            }
        }
        return Err(test_error(format!(
            "{error}; agent projects artifacts: {}",
            artifacts.display()
        )));
    }
    outcome
}
