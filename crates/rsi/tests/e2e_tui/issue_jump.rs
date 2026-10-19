//! Go to an Issue by number (#2381): the operator types `#N` (or `N`) into the
//! tracker, presses `#`, or runs `:issue N`, and lands on that Issue even when
//! it is Closed and Archived; an unknown number says so. The same module covers
//! the tracker's one-key operator filter and the detail's folding sections and
//! related-Issue links (#2381).
use super::*;
use rsi_common::issue_workspace::{ArchiveIssueRequestV1, UpdateIssueStatusV2RequestV1};
use rsi_common::types::IssueStatus;

const ARCHIVED_TITLE: &str = "Needle archived tracker issue";

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("issue jump key: {e:?}")))
}

async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    term.expect(text)
        .timeout(Duration::from_secs(15))
        .await
        .map_err(|e| test_error(format!("expected {text:?}: {e:?}")))
}

async fn type_text(term: &Terminal, text: &str) -> E2eResult<()> {
    for c in text.chars() {
        press(term, Key::Char(c)).await?;
    }
    Ok(())
}

/// Seed the standard fixture, then close and archive one more Issue so the
/// default Active view cannot show it. Returns its number.
async fn seed_archived_issue(h: &mut E2eHarness, client: &mut DaemonClient) -> E2eResult<i64> {
    h.seed_fixture(client).await?;
    let project_id = h
        .fixture
        .project_id
        .ok_or_else(|| test_error("fixture project missing"))?;
    let created = client
        .create_issue_v2(CreateIssueV2RequestV1 {
            project_id,
            title: ARCHIVED_TITLE.to_string(),
            body: "Found through a number, never through the default list".to_string(),
            priority: Some(3),
            labels: vec!["e2e".to_string()],
            assignee: None,
            idempotency_key: "e2e-issue-jump-archived-v1".to_string(),
        })
        .await?
        .issue;
    let closed = client
        .update_issue_status_v2(UpdateIssueStatusV2RequestV1 {
            project_id,
            issue_id: created.id,
            expected_row_version: created.row_version,
            idempotency_key: "e2e-issue-jump-close-v1".to_string(),
            status: IssueStatus::Closed,
        })
        .await?
        .issue;
    client
        .archive_issue(ArchiveIssueRequestV1 {
            project_id,
            issue_id: closed.id,
            expected_row_version: closed.row_version,
            idempotency_key: "e2e-issue-jump-archive-v1".to_string(),
        })
        .await?;
    Ok(created.display_number)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_e2e_issue_jump_by_number_reaches_archived_issues() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    h.phase = "issue-jump-daemon";
    h.start_daemon()?;
    let mut client = h.wait_for_daemon().await?;
    let seeded = seed_archived_issue(&mut h, &mut client).await;
    client.disconnect();
    let archived_number = seeded?;
    let open_number = h
        .fixture
        .issue_number
        .ok_or_else(|| test_error("fixture issue number missing"))?;

    let term = Terminal::builder()
        .size(PTY_COLS, PTY_ROWS)
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
        .map_err(|e| test_error(format!("issue jump TUI: {e:?}")))?;
    let outcome = async {
        visible(&term, "Sessions").await?;
        // Enter the fixture project so the Issues workspace is project-scoped.
        press(&term, Key::Char(' ')).await?;
        press(&term, Key::Char('p')).await?;
        visible(&term, "Workspaces").await?;
        visible(&term, PROJECT_NAME).await?;
        press(&term, Key::Char('j')).await?;
        press(&term, Key::Enter).await?;
        visible(&term, GROUP_NAME).await?;
        press(&term, Key::Char(' ')).await?;
        press(&term, Key::Char('i')).await?;
        visible(&term, ISSUE_TITLE).await?;

        // The default view hides the archived Issue; `#` then its number
        // opens it anyway.
        press(&term, Key::Char('#')).await?;
        visible(&term, "Go to Issue").await?;
        type_text(&term, &archived_number.to_string()).await?;
        press(&term, Key::Enter).await?;
        visible(&term, ARCHIVED_TITLE).await?;
        visible(&term, &format!("Inspector · #{archived_number}")).await?;

        // An unknown number says so.
        press(&term, Key::Char('#')).await?;
        type_text(&term, "999999").await?;
        press(&term, Key::Enter).await?;
        visible(&term, "No Issue #999999").await?;

        // `/` with a bare number is the same jump, back to the open Issue.
        press(&term, Key::Char('/')).await?;
        for _ in 0..16 {
            press(&term, Key::Backspace).await?;
        }
        type_text(&term, &format!("#{open_number}")).await?;
        press(&term, Key::Enter).await?;
        visible(&term, &format!("Inspector · #{open_number}")).await?;

        // `:issue N` reaches the archived one again.
        type_text(&term, &format!(":issue {archived_number}")).await?;
        press(&term, Key::Enter).await?;
        visible(&term, &format!("Inspector · #{archived_number}")).await?;
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;
    h.final_screen_text = Some(term.screen().await.text());
    let _ = term.kill().await;
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; issue jump artifacts: {}",
            artifacts.display()
        )));
    }
    Ok(())
}

const OPERATOR_TITLE: &str = "Operator request needle for the tracker";

/// Seed the fixture plus one operator-request Issue whose body folds a handoff
/// and mentions the fixture Issue. Returns the operator Issue's number.
async fn seed_operator_issue(
    h: &mut E2eHarness,
    client: &mut DaemonClient,
    mentioned: i64,
) -> E2eResult<i64> {
    let project_id = h
        .fixture
        .project_id
        .ok_or_else(|| test_error("fixture project missing"))?;
    let created = client
        .create_issue_v2(CreateIssueV2RequestV1 {
            project_id,
            title: OPERATOR_TITLE.to_string(),
            body: format!(
                "Asked for by the operator, see #{mentioned}.\n\n## Handoff\nRESULT abc123 done\n"
            ),
            priority: Some(1),
            labels: vec!["operator-request".to_string()],
            assignee: None,
            idempotency_key: "e2e-issue-operator-v1".to_string(),
        })
        .await?
        .issue;
    Ok(created.display_number)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_e2e_issue_tracker_operator_filter_and_detail_links() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    h.phase = "issue-tracker-daemon";
    h.start_daemon()?;
    let mut client = h.wait_for_daemon().await?;
    let seeded = async {
        h.seed_fixture(&mut client).await?;
        let mentioned = h
            .fixture
            .issue_number
            .ok_or_else(|| test_error("fixture issue number missing"))?;
        seed_operator_issue(&mut h, &mut client, mentioned).await
    }
    .await;
    client.disconnect();
    let operator_number = seeded?;
    let open_number = h
        .fixture
        .issue_number
        .ok_or_else(|| test_error("fixture issue number missing"))?;

    let term = Terminal::builder()
        .size(PTY_COLS, PTY_ROWS)
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
        .map_err(|e| test_error(format!("issue tracker TUI: {e:?}")))?;
    let outcome = async {
        visible(&term, "Sessions").await?;
        press(&term, Key::Char(' ')).await?;
        press(&term, Key::Char('p')).await?;
        visible(&term, "Workspaces").await?;
        visible(&term, PROJECT_NAME).await?;
        press(&term, Key::Char('j')).await?;
        press(&term, Key::Enter).await?;
        visible(&term, GROUP_NAME).await?;
        press(&term, Key::Char(' ')).await?;
        press(&term, Key::Char('i')).await?;
        visible(&term, ISSUE_TITLE).await?;
        visible(&term, OPERATOR_TITLE).await?;

        // One key keeps only the operator-asked Issues, and the list says so.
        press(&term, Key::Char('O')).await?;
        visible(&term, "operator only").await?;
        visible(&term, OPERATOR_TITLE).await?;

        // The detail folds the handoff and lists the mentioned Issue.
        press(&term, Key::Enter).await?;
        visible(&term, &format!("Inspector · #{operator_number}")).await?;
        visible(&term, "OPERATOR REQUEST").await?;
        visible(&term, "HANDOFFS AND REVIEWS").await?;
        visible(&term, "RELATED").await?;

        // `o` opens the folded section.
        press(&term, Key::Char('o')).await?;
        visible(&term, "RESULT abc123 done").await?;

        // `}` then `o` follows the mention; `B` comes back.
        press(&term, Key::Char('}')).await?;
        press(&term, Key::Char('o')).await?;
        visible(&term, &format!("Inspector · #{open_number}")).await?;
        press(&term, Key::Char('B')).await?;
        visible(&term, &format!("Inspector · #{operator_number}")).await?;
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;
    h.final_screen_text = Some(term.screen().await.text());
    let _ = term.kill().await;
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; issue tracker artifacts: {}",
            artifacts.display()
        )));
    }
    Ok(())
}
