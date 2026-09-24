//! Real operator persistence/keyboard flow with an isolated scripted manager.
//! Provider intelligence and root succession are verified separately.
use super::*;
use futures::FutureExt;
use rsi_common::{harness_manager::ConfigureHarnessManagerRequestV1, harness_manager_v2::*};
use serde_json::{Value, json};
use std::panic::AssertUnwindSafe;

fn assertion_error(
    panic: Box<dyn std::any::Any + Send>,
) -> Box<dyn std::error::Error + Send + Sync> {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("non-string assertion panic");
    test_error(format!("manager fixture assertion failed: {message}"))
}

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("manager key: {e:?}")))
}

async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    term.expect(text)
        .timeout(Duration::from_secs(8))
        .await
        .map_err(|e| test_error(format!("expected {text:?}: {e:?}")))
}

async fn command(term: &Terminal, text: &str) -> E2eResult<()> {
    for c in format!(":{text}").chars() {
        press(term, Key::Char(c)).await?;
    }
    press(term, Key::Enter).await
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

async fn stop_fixture_sessions(h: &E2eHarness, fixture: &Value) -> E2eResult<()> {
    let ids = ["manager_session_id", "created_child_session_id"]
        .into_iter()
        .filter_map(|key| fixture[key].as_str())
        .map(uuid::Uuid::parse_str)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Ok(());
    }
    let mut client = DaemonClient::new(h.socket_path.clone());
    client.connect().await?;
    let active = |status| {
        matches!(
            status,
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
        )
    };
    for id in ids {
        if client
            .list_sessions()
            .await?
            .iter()
            .any(|session| session.id == id && active(session.status))
        {
            // Only daemon-returned identities from this isolated fixture are
            // eligible. Recheck a terminal race before treating refusal as a
            // cleanup failure; retain any still-active process as a red test.
            if let Err(error) = client.interrupt_session(id).await {
                if client
                    .list_sessions()
                    .await?
                    .iter()
                    .any(|session| session.id == id && active(session.status))
                {
                    return Err(error.into());
                }
            }
        }
    }
    client.disconnect();
    Ok(())
}

#[tokio::test]
async fn test_e2e_manager_presets_save_task_and_custom_reopen() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut harness = E2eHarness::new()?;
    let mut fixture = json!({
        "project": {"id": null, "name": "Manager preset fixture"},
        "group": {"id": null, "name": "Manager fixture group", "kind": "Group"},
        "epic": {"id": null, "name": "Manager fixture Epic", "kind": "Epic", "parent_id": null},
        "manager_session_id": null,
        "created_child_session_id": null,
        "tags": ["e2e"],
        "provider": "isolated scripted Codex protocol fixture; no model inference",
        "pty": {"columns": 150, "rows": 46}
    });
    let mut result = AssertUnwindSafe(run(&mut harness, &mut fixture))
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| Err(assertion_error(panic)));
    let cleanup = tokio::time::timeout(
        Duration::from_secs(10),
        stop_fixture_sessions(&harness, &fixture),
    )
    .await
    .map_err(|error| test_error(format!("fixture cleanup timeout: {error}")))
    .and_then(|result| result);
    if let Err(error) = cleanup {
        fixture["cleanup_error"] = json!(error.to_string());
        result = Err(test_error(match result {
            Ok(()) => format!("fixture cleanup failed: {error}"),
            Err(original) => format!("{original}; fixture cleanup failed: {error}"),
        }));
    }
    harness.shutdown_daemon();
    let failure = result.as_ref().err().map(ToString::to_string);
    let artifacts = harness.preserve_artifacts(failure.as_deref())?;
    fixture["phase"] = json!(harness.phase);
    fs::write(
        artifacts.join("fixture.json"),
        serde_json::to_vec_pretty(&fixture)?,
    )?;
    fs::write(
        artifacts.join("scenario.txt"),
        format!(
            "Scenario: Save Execute -> save Full project control -> authenticated task creation -> Custom reopen and save\nTimestamp: {}\nPhase: {}\nPTY: 150x46\nProvider: isolated scripted Codex protocol fixture; no model inference or root succession proof\nStatus: {}\n{}",
            chrono::Utc::now().to_rfc3339(),
            harness.phase,
            if result.is_ok() { "SUCCESS" } else { "FAILED" },
            failure
                .as_ref()
                .map(|error| format!("Error: {error}\n"))
                .unwrap_or_default(),
        ),
    )?;
    for name in [
        "manager-policy.json",
        "manager-action-result.json",
        "manager-action.json",
        "manager-child.json",
        "execute-screen.txt",
        "full-screen.txt",
        "custom-screen.txt",
    ] {
        let source = harness.artifacts_dir.join(name);
        if source.exists() {
            fs::copy(source, artifacts.join(name))?;
        }
    }
    result
}

async fn run(h: &mut E2eHarness, fixture: &mut Value) -> E2eResult<()> {
    let repo = h.home_dir.join("manager-repo");
    fs::create_dir(&repo)?;
    git(&repo, &["init", "-b", "rolling"])?;
    git(&repo, &["config", "user.name", "Fixture"])?;
    git(&repo, &["config", "user.email", "fixture@example.invalid"])?;
    fs::write(repo.join("fixture.txt"), "manager policy fixture\n")?;
    git(&repo, &["add", "fixture.txt"])?;
    git(&repo, &["commit", "-m", "fixture"])?;
    fs::write(
        h.bin_dir.join("codex"),
        include_bytes!("../fixtures/manager-preset-actor.py"),
    )?;
    fs::set_permissions(h.bin_dir.join("codex"), fs::Permissions::from_mode(0o755))?;
    h.phase = "manager-presets-daemon";
    h.start_daemon()?;
    let mut client = h.wait_for_daemon().await?;
    let project = client
        .create_project("Manager preset fixture", Some(&repo), None, None)
        .await?;
    fixture["project"]["id"] = json!(project.id);
    let group = client
        .create_container(
            SessionKind::Group,
            "Manager fixture group",
            None,
            Some(project.id),
            &["e2e".into()],
            None,
        )
        .await?;
    fixture["group"]["id"] = json!(group);
    let epic = client
        .create_container(
            SessionKind::Epic,
            "Manager fixture Epic",
            Some(group),
            Some(project.id),
            &["e2e".into()],
            None,
        )
        .await?;
    h.fixture.group_id = Some(group);
    h.fixture.epic_id = Some(epic);
    fixture["epic"]["id"] = json!(epic);
    fixture["epic"]["parent_id"] = json!(group);
    client
        .launch_session_with_opts(
            "MANAGER_PRESET_E2E_ACTOR",
            Some("Manager preset actor"),
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
            .find(|s| s.title.as_deref() == Some("Manager preset actor"));
        if manager
            .as_ref()
            .is_some_and(|s| s.status == SessionStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let manager = manager.ok_or_else(|| test_error("manager fixture missing"))?;
    fixture["manager_session_id"] = json!(manager.id);
    assert_eq!(manager.status, SessionStatus::Running);
    assert_eq!(manager.parent_id, None);
    let config = client
        .configure_harness_manager(ConfigureHarnessManagerRequestV1 {
            project_id: project.id,
            session_id: manager.id,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .await?;
    assert!(
        client
            .get_harness_manager_policy(project.id)
            .await?
            .is_none()
    );
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
    let scenario = AssertUnwindSafe(async {
        visible(&term, "Manager preset actor").await?;
        command(&term, "manager policy").await?;
        visible(&term, "Saved: no policy").await?;
        press(&term, Key::Char('j')).await?;
        press(&term, Key::Enter).await?;
        visible(&term, "Draft: Execute").await?;
        assert!(
            client
                .get_harness_manager_policy(project.id)
                .await?
                .is_none()
        );
        press(&term, Key::Char('s')).await?;
        visible(&term, "Saved: Execute").await?;
        let execute = client
            .get_harness_manager_policy(project.id)
            .await?
            .unwrap();
        assert_eq!(execute.row_version, 1);
        assert_eq!(execute.policy.mode, ManagerOperatingModeV2::Execute);
        assert_eq!(
            (
                execute.policy.max_created_containers,
                execute.policy.max_created_sessions,
                execute.policy.max_recovery_attempts
            ),
            (8, 32, 3)
        );
        assert_eq!(execute.policy.max_active_sessions, 4);
        assert_eq!(execute.policy.allowed_launches, vec![]);
        assert!(
            execute
                .policy
                .capabilities
                .contains(&ManagerCapabilityV2::SelfSuccession)
        );
        assert_eq!(execute.policy.capabilities.len(), 6);
        assert!(!execute.policy.allow_create_groups);
        fs::write(
            h.artifacts_dir.join("execute-screen.txt"),
            term.screen().await.text(),
        )?;
        press(&term, Key::Char('j')).await?;
        press(&term, Key::Enter).await?;
        visible(&term, "Draft: Full project control").await?;
        assert_eq!(
            client
                .get_harness_manager_policy(project.id)
                .await?
                .unwrap()
                .row_version,
            1
        );
        press(&term, Key::Char('s')).await?;
        visible(&term, "Saved: Full project control").await?;
        let full = client
            .get_harness_manager_policy(project.id)
            .await?
            .unwrap();
        assert_eq!(full.row_version, 2);
        assert_eq!(full.policy.capabilities.len(), 7);
        assert!(full.policy.allow_create_groups);
        assert_eq!(
            (
                full.policy.max_created_containers,
                full.policy.max_created_sessions,
                full.policy.max_recovery_attempts
            ),
            (8, 32, 3)
        );
        fs::write(
            h.artifacts_dir.join("full-screen.txt"),
            term.screen().await.text(),
        )?;

        h.phase = "manager-presets-authenticated-task";
        // The fixture provider uses its daemon-minted token only in the RPC
        // transport. This file contains public action parameters, no token.
        fs::write(
            h.bin_dir.join("manager-action.pending"),
            serde_json::to_vec(&json!({
                "fence":{"scope_version":config.row_version,"policy_version":full.row_version},
                "idempotency_key":"manager-presets-real-task",
                "operation":{"action":"create_session","parent_id":epic,"kind":"Task",
                    "query":"Manager preset child fixture",
                    "launch":{"provider":"Codex","model":"gpt-6-astra","effort":"medium"}}
            }))?,
        )?;
        fs::rename(
            h.bin_dir.join("manager-action.pending"),
            h.bin_dir.join("manager-action.json"),
        )?;
        let mut action = None;
        for _ in 0..300 {
            let page = client
                .get_harness_manager_state(GetHarnessManagerStateRequestV2 {
                    project_id: project.id,
                    query: AgentManagerInspectRequestV2 {
                        section: ManagerInspectSectionV2::Actions,
                        ..Default::default()
                    },
                })
                .await?;
            action = page
                .rows
                .into_iter()
                .find(|r| r.pointer("/operation/action") == Some(&json!("create_session")));
            if action.as_ref().is_some_and(|a| {
                ["succeeded", "blocked", "failed", "uncertain", "revoked"]
                    .contains(&a["state"].as_str().unwrap_or_default())
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let receipt: Value =
            serde_json::from_slice(&fs::read(h.bin_dir.join("manager-action-result.json"))?)?;
        fs::write(
            h.artifacts_dir.join("manager-action-result.json"),
            serde_json::to_vec_pretty(&receipt)?,
        )?;
        let action = action.ok_or_else(|| {
            test_error(format!("manager action missing; fixture receipt {receipt}"))
        })?;
        fs::write(
            h.artifacts_dir.join("manager-action.json"),
            serde_json::to_vec_pretty(&action)?,
        )?;
        assert_eq!(
            action["state"], "succeeded",
            "action: {action}; fixture receipt: {receipt}"
        );
        let children = client.list_sessions().await?;
        let child = children
            .iter()
            .find(|s| s.parent_id == Some(epic) && s.query == "Manager preset child fixture")
            .ok_or_else(|| test_error("manager-created task missing"))?;
        fixture["created_child_session_id"] = json!(child.id);
        fs::write(
            h.artifacts_dir.join("manager-child.json"),
            serde_json::to_vec_pretty(&json!({
                "id": child.id, "parent_id": child.parent_id, "kind": child.session_kind,
                "provider": child.provider, "model": child.model, "effort": child.effort,
                "status": child.status, "sandbox_root": child.sandbox_root,
                "sandbox_branch": child.sandbox_branch,
            }))?,
        )?;
        assert_eq!(child.session_kind, SessionKind::Task);
        assert_eq!(child.provider, SessionProvider::Codex);
        assert_eq!(child.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(child.effort.as_deref(), Some("medium"));
        assert_ne!(child.sandbox_root, manager.sandbox_root);

        h.phase = "manager-presets-custom-reopen";
        let mut custom = full.policy.clone();
        custom.max_created_containers = 0;
        custom.max_created_sessions = 17;
        custom.max_recovery_attempts = 0;
        custom.paused = true;
        custom.paused_epic_ids = vec![epic];
        custom.max_spend_usd = Some(25.0);
        custom.provider_limits = vec![
            ManagerProviderLimitV2 {
                provider: SessionProvider::Codex,
                max_active: 2,
            },
            ManagerProviderLimitV2 {
                provider: SessionProvider::Local,
                max_active: 1,
            },
        ];
        custom.allowed_launches = vec![
            ManagerLaunchChoiceV2 {
                provider: SessionProvider::Codex,
                model: "retained-legacy-choice".into(),
                effort: None,
            },
            ManagerLaunchChoiceV2 {
                provider: SessionProvider::Codex,
                model: "gpt-6-astra".into(),
                effort: Some("medium".into()),
            },
        ];
        custom.capabilities.reverse();
        let saved = client
            .configure_harness_manager_policy(ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project.id,
                expected_scope_version: config.row_version,
                expected_policy_version: full.row_version,
                idempotency_key: "manager-presets-custom-fixture".into(),
                policy: custom.clone(),
            })
            .await?;
        press(&term, Key::Char('q')).await?;
        command(&term, "manager policy").await?;
        visible(&term, "Saved: Custom").await?;
        visible(&term, "Draft: Custom").await?;
        visible(&term, "retained-legacy-choice").await?;
        press(&term, Key::Char('s')).await?;
        visible(&term, &format!("policy {}", saved.row_version + 1)).await?;
        let reopened = client
            .get_harness_manager_policy(project.id)
            .await?
            .unwrap();
        assert_eq!(reopened.policy, custom);
        assert_eq!(reopened.row_version, saved.row_version + 1);
        fs::write(
            h.artifacts_dir.join("manager-policy.json"),
            serde_json::to_vec_pretty(&reopened)?,
        )?;
        fs::write(
            h.artifacts_dir.join("custom-screen.txt"),
            term.screen().await.text(),
        )?;
        press(&term, Key::Char('q')).await?;
        command(&term, "manager policy").await?;
        visible(&term, "Saved: Custom").await?;
        assert_eq!(
            client
                .get_harness_manager_policy(project.id)
                .await?
                .unwrap()
                .policy,
            custom
        );
        Ok(())
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|panic| Err(assertion_error(panic)));
    h.final_screen_text = Some(term.screen().await.text());
    let _ = term.kill().await;
    client.disconnect();
    scenario
}
