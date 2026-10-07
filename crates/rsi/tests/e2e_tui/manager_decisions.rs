//! Operator decisions board (`:manager decisions`, #1428): a seeded manager
//! project with live, answered and stale decision records, driven by real
//! keystrokes. Records are seeded straight into the daemon's store because a
//! provider-backed manager is out of scope; the board reads them through the
//! real `GetHarnessManagerState` RPC and answers through the real
//! `AnswerHarnessManagerDecision` RPC.
use super::*;
use rsi_common::{harness_manager::ConfigureHarnessManagerRequestV1, harness_manager_v2::*};
use serde_json::{Value, json};
use uuid::Uuid;

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("decisions key: {e:?}")))
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

/// Save the current screen as text and PNG under `RSI_E2E_SHOT_DIR` (or the
/// run's artifact dir) so before/after evidence survives the run.
async fn shot(h: &E2eHarness, term: &Terminal, name: &str) -> E2eResult<()> {
    let dir = std::env::var("RSI_E2E_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| h.artifacts_dir.clone());
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(format!("{name}.txt")), term.screen().await.text())?;
    let png = term
        .screenshot()
        .await
        .to_png()
        .map_err(|e| test_error(format!("screenshot png: {e:?}")))?;
    fs::write(dir.join(format!("{name}.png")), png)?;
    Ok(())
}

pub(super) struct Fixture {
    pub client: DaemonClient,
    pub project_id: Uuid,
    pub scope_version: i64,
    pub epic: Uuid,
    pub manager: rsi_common::types::Session,
    pub db: PathBuf,
    _socket_dir: tempfile::TempDir,
}

impl Fixture {
    fn conn(&self) -> E2eResult<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(&self.db)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Seed one `decision` record the way a manager's declaration stores it.
    pub fn seed(
        &self,
        key: &str,
        status: &str,
        created_at: &str,
        question: &str,
        answer: Option<&str>,
    ) -> E2eResult<()> {
        self.seed_with(key, status, created_at, question, answer, json!({}))
    }

    /// `seed` plus extra stored fields (`options`, `answered_by`, `history`,
    /// `gate`, ...) merged into the record payload as the daemon writes them.
    pub fn seed_with(
        &self,
        key: &str,
        status: &str,
        created_at: &str,
        question: &str,
        answer: Option<&str>,
        extra: Value,
    ) -> E2eResult<()> {
        let mut payload = json!({
            "key": key, "epic_id": self.epic, "question": question,
            "request_id": null, "work_key": null,
            "target_digest": format!("digest-{key}"), "target_row_version": null,
            "status": status, "answer": answer, "delivery": null,
        });
        if let (Some(fields), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            fields.extend(extra.clone());
        }
        self.conn()?.execute(
            "INSERT INTO harness_manager_v2_records
             (project_id,manager_session_id,scope_version,kind,record_key,epic_id,row_version,
              payload_json,archived,created_at,updated_at)
             VALUES (?1,?2,?3,'decision',?4,?5,1,?6,0,?7,?7)",
            rusqlite::params![
                self.project_id.to_string(),
                self.manager.id.to_string(),
                self.scope_version,
                key,
                self.epic.to_string(),
                payload.to_string(),
                created_at
            ],
        )?;
        Ok(())
    }

    /// `(status, archived)` of one decision record, read straight from the
    /// store; `None` when the row is gone (archiving must never delete it).
    pub fn archived(&self, key: &str) -> E2eResult<Option<(String, bool)>> {
        let row = self.conn()?.query_row(
            "SELECT payload_json, archived FROM harness_manager_v2_records
             WHERE project_id=?1 AND kind='decision' AND record_key=?2",
            rusqlite::params![self.project_id.to_string(), key],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        );
        match row {
            Ok((payload, archived)) => {
                let payload: Value = serde_json::from_str(&payload)?;
                Ok(Some((
                    payload["status"].as_str().unwrap_or_default().to_string(),
                    archived != 0,
                )))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// `(status, answer)` of one decision record, read straight from the store.
    pub fn record(&self, key: &str) -> E2eResult<(String, Option<String>)> {
        let payload: String = self.conn()?.query_row(
            "SELECT payload_json FROM harness_manager_v2_records
             WHERE project_id=?1 AND kind='decision' AND record_key=?2",
            rusqlite::params![self.project_id.to_string(), key],
            |r| r.get(0),
        )?;
        let payload: Value = serde_json::from_str(&payload)?;
        Ok((
            payload["status"].as_str().unwrap_or_default().to_string(),
            payload["answer"].as_str().map(str::to_string),
        ))
    }
}

fn ago(minutes: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(minutes))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

pub(super) const CENSUS_QUESTION: &str = "Census access: how should E3 read the census data?\n\
A) Read the census database directly\n\
B) Call the census HTTP API\n\
C) Read a nightly mirror\n\
Recommend: A";
pub(super) const INGEST_QUESTION: &str = "Ingest contact: who owns the ingest handoff?\n\
1. The platform team\n\
2. The data team (recommended)\n\
3. Nobody yet";

/// A manager project (one Epic, one real manager session, an operator policy)
/// with the decision records every decisions-board scenario starts from.
pub(super) async fn setup(h: &mut E2eHarness, tag: &str) -> E2eResult<Fixture> {
    let socket_dir = tempfile::Builder::new()
        .prefix("rsi-md-")
        .tempdir_in("/tmp")?;
    h.socket_path = socket_dir.path().join("daemon.sock");
    let repo = h.home_dir.join(format!("{tag}-repo"));
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
    fs::write(repo.join("fixture.txt"), "decisions fixture\n")?;
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
    h.phase = "manager-decisions-daemon";
    h.start_daemon()?;
    let mut client = h.wait_for_daemon().await?;
    let project = client
        .create_project("Decisions fixture", Some(&repo), None, None)
        .await?;
    let group = client
        .create_container(
            SessionKind::Group,
            "Decisions Group",
            None,
            Some(project.id),
            &["e2e".into()],
            None,
        )
        .await?;
    let epic = client
        .create_container(
            SessionKind::Epic,
            "Census Epic",
            Some(group),
            Some(project.id),
            &["e2e".into()],
            None,
        )
        .await?;
    client
        .launch_session_with_opts(
            "MANAGER_PRESET_E2E_ACTOR",
            Some("Decisions manager"),
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
            .find(|s| s.title.as_deref() == Some("Decisions manager"));
        if manager
            .as_ref()
            .is_some_and(|s| s.status == SessionStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let manager = manager.ok_or_else(|| test_error("decisions manager session missing"))?;
    client
        .configure_harness_manager(ConfigureHarnessManagerRequestV1 {
            project_id: project.id,
            session_id: manager.id,
            epic_ids: Some(vec![epic]),
            group_ids: vec![],
            expected_row_version: 0,
        })
        .await?;
    let config = client
        .get_harness_manager(project.id)
        .await?
        .ok_or_else(|| test_error("manager config missing"))?;
    client
        .configure_harness_manager_policy(ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project.id,
            expected_scope_version: config.row_version,
            expected_policy_version: 0,
            idempotency_key: format!("decisions-policy-{tag}"),
            policy: ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        })
        .await?;
    let fixture = Fixture {
        client,
        project_id: project.id,
        scope_version: config.row_version,
        epic,
        manager,
        db: socket_dir.path().join("rsi.db"),
        _socket_dir: socket_dir,
    };
    fixture.seed("census-access", "pending", &ago(125), CENSUS_QUESTION, None)?;
    fixture.seed("ingest-contact", "pending", &ago(7), INGEST_QUESTION, None)?;
    fixture.seed(
        "free-form-note",
        "pending",
        &ago(30),
        "Name the on-call contact for the census cutover.",
        None,
    )?;
    fixture.seed(
        "D24-integration-target-custody",
        "pending",
        "2026-09-12T10:00:00.000000000Z",
        "Who holds integration-target custody?\nA) the manager\nB) the operator",
        None,
    )?;
    fixture.seed(
        "retired-tooling",
        "answered",
        &ago(2000),
        "Retire the old tooling?\nA) yes\nB) no\nRecommend: A",
        Some("A: yes"),
    )?;
    Ok(fixture)
}

async fn open_term(h: &E2eHarness) -> E2eResult<Terminal> {
    Terminal::builder()
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
        .map_err(|e| test_error(format!("decisions TUI: {e:?}")))
}

/// Run `scenario` against a fresh fixture, then always tear the daemon down and
/// preserve artifacts on failure.
async fn run<F>(name: &'static str, scenario: F) -> E2eResult<()>
where
    F: for<'a> FnOnce(
        &'a mut E2eHarness,
        &'a Terminal,
        &'a mut Fixture,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = E2eResult<()>> + 'a>>,
{
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let mut fixture = setup(&mut h, name).await?;
    let term = open_term(&h).await?;
    let outcome = async {
        visible(&term, "Decisions manager").await?;
        command(&term, "manager decisions").await?;
        visible(&term, "[2 Decisions]").await?;
        scenario(&mut h, &term, &mut fixture).await
    }
    .await;
    h.final_screen_text = Some(term.screen().await.text());
    let _ = term.send_key(Key::Ctrl('c')).await;
    let manager_id = fixture.manager.id;
    let _ = fixture.client.interrupt_session(manager_id).await;
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; decisions artifacts: {}",
            artifacts.display()
        )));
    }
    Ok(())
}

/// The board as the operator sees it with a mixed queue: live records lead,
/// the stale September record and the closed one follow.
#[tokio::test]
async fn test_e2e_manager_decisions_board_layout() -> E2eResult<()> {
    run("layout", |h, term, _fixture| {
        Box::pin(async move {
            visible(term, "NEEDS YOU 3").await?;
            visible(term, "STALE 1").await?;
            visible(term, "CLOSED 1").await?;
            visible(term, "Ingest contact").await?;
            shot(h, term, "decisions-board").await?;
            // Longest-waiting live record first: census-access (2h) is selected
            // and shows its options with the recommendation starred.
            visible(term, "A  Read the census database directly").await?;
            visible(term, "★ recommended").await?;
            visible(term, "HISTORY").await?;
            press(term, Key::Char('j')).await?;
            visible(term, "Name the on-call contact").await?;
            press(term, Key::Char('j')).await?;
            visible(term, "2  The data team").await?;
            shot(h, term, "decisions-board-numbered").await?;
            Ok(())
        })
    })
    .await
}

/// `Enter` answers the highlighted option: move to B, press Enter, and the
/// record settles with that answer and drops out of NEEDS YOU.
#[tokio::test]
async fn test_e2e_manager_decisions_enter_answers_the_selected_option() -> E2eResult<()> {
    run("answer-option", |h, term, fixture| {
        Box::pin(async move {
            visible(term, "› A  Read the census database directly").await?;
            press(term, Key::Char('l')).await?;
            visible(term, "› B  Call the census HTTP API").await?;
            press(term, Key::Enter).await?;
            visible(term, "Answered census-access").await?;
            visible(term, "NEEDS YOU 2").await?;
            shot(h, term, "decisions-after-answer").await?;
            let (status, answer) = fixture.record("census-access")?;
            assert_eq!(status, "answered");
            assert_eq!(answer.as_deref(), Some("B: Call the census HTTP API"));
            Ok(())
        })
    })
    .await
}

/// `y` accepts the asker's recommendation without touching the option cursor.
#[tokio::test]
async fn test_e2e_manager_decisions_y_accepts_the_recommendation() -> E2eResult<()> {
    run("accept-recommendation", |_h, term, fixture| {
        Box::pin(async move {
            // ingest-contact recommends option 2 via a (recommended) marker.
            press(term, Key::Char('j')).await?;
            press(term, Key::Char('j')).await?;
            visible(term, "2  The data team").await?;
            press(term, Key::Char('y')).await?;
            visible(term, "Answered ingest-contact").await?;
            let (status, answer) = fixture.record("ingest-contact")?;
            assert_eq!(status, "answered");
            assert_eq!(answer.as_deref(), Some("2: The data team"));
            // A decision without a recommendation refuses `y` and stays pending.
            press(term, Key::Char('k')).await?;
            press(term, Key::Char('y')).await?;
            visible(term, "no recommendation").await?;
            assert_eq!(fixture.record("free-form-note")?.0, "pending");
            Ok(())
        })
    })
    .await
}

/// A free-text decision opens the answer prompt on `Enter`.
#[tokio::test]
async fn test_e2e_manager_decisions_free_text_answer() -> E2eResult<()> {
    run("free-text", |_h, term, fixture| {
        Box::pin(async move {
            press(term, Key::Char('j')).await?;
            visible(term, "Name the on-call contact").await?;
            press(term, Key::Enter).await?;
            visible(term, "Answering free-form-note").await?;
            for c in "Dana, 555-0100".chars() {
                press(term, Key::Char(c)).await?;
            }
            press(term, Key::Enter).await?;
            visible(term, "Answered free-form-note").await?;
            let (status, answer) = fixture.record("free-form-note")?;
            assert_eq!(status, "answered");
            assert_eq!(answer.as_deref(), Some("Dana, 555-0100"));
            Ok(())
        })
    })
    .await
}

fn actor(kind: &str, label: &str) -> Value {
    json!({"kind": kind, "session_id": Uuid::new_v4(), "node_label": label, "at": ago(20)})
}

/// A delegated manager's ruling: the board says who ruled and that the
/// operator did not answer, in both the list and the detail.
#[tokio::test]
async fn test_e2e_manager_decisions_shows_a_delegated_ruling() -> E2eResult<()> {
    run("delegated-ruling", |h, term, fixture| {
        Box::pin(async move {
            let ruler = actor("portfolio_manager", "Global manager");
            fixture.seed_with(
                "census-ruled",
                "answered",
                &ago(20),
                "Census mirror: who pays for it?\nA) Platform\nB) Data",
                Some("A: Platform"),
                json!({"answered_by": ruler, "history": [
                    {"event":"asked","at":ago(20),"actor":actor("project_manager","Census PM")},
                    {"event":"ruled","at":ago(20),"actor":ruler,"note":"portfolio_above_owner"}]}),
            )?;
            press(term, Key::Char('r')).await?;
            visible(term, "CLOSED 2").await?;
            // Newest settled record leads CLOSED: NEEDS YOU 3, STALE 1, then it.
            for _ in 0..4 {
                press(term, Key::Char('j')).await?;
            }
            visible(term, "ruled by Global manager").await?;
            visible(term, "Ruled by: Global manager").await?;
            visible(term, "ruled by Global manager").await?;
            visible(term, "ruled by Global manager: portfolio_above_owner").await?;
            shot(h, term, "decisions-delegated-ruling").await?;
            Ok(())
        })
    })
    .await
}

/// A manager-withdrawn record is its own status, not "needs answer".
#[tokio::test]
async fn test_e2e_manager_decisions_shows_a_withdrawn_record() -> E2eResult<()> {
    run("withdrawn", |h, term, fixture| {
        Box::pin(async move {
            fixture.seed_with(
                "superseded-plan",
                "withdrawn",
                &ago(60),
                "Adopt the old ingest plan?\nA) yes\nB) no",
                None,
                json!({"history": [
                    {"event":"asked","at":ago(60),"actor":actor("project_manager","Ingest PM")},
                    {"event":"withdrawn","at":ago(45),"actor":actor("project_manager","Ingest PM"),
                     "note":"superseded by the new plan"}]}),
            )?;
            press(term, Key::Char('r')).await?;
            visible(term, "CLOSED 2").await?;
            for _ in 0..4 {
                press(term, Key::Char('j')).await?;
            }
            visible(term, "⊘ withdrawn").await?;
            visible(term, "withdrawn by Ingest PM: superseded by the new plan").await?;
            shot(h, term, "decisions-withdrawn").await?;
            Ok(())
        })
    })
    .await
}

/// The daemon's structured options drive the answer: the label and detail
/// are recorded, and the recommendation is where Enter starts.
#[tokio::test]
async fn test_e2e_manager_decisions_structured_options_answer() -> E2eResult<()> {
    run("structured-options", |_h, term, fixture| {
        Box::pin(async move {
            fixture.seed_with(
                "mirror-choice",
                "pending",
                &ago(1),
                "Which mirror do we trust?",
                None,
                json!({"options": [
                    {"label":"Primary","detail":"the live replica","recommended":false},
                    {"label":"Nightly","detail":"yesterday's copy","recommended":true}]}),
            )?;
            press(term, Key::Char('r')).await?;
            visible(term, "NEEDS YOU 4").await?;
            for _ in 0..3 {
                press(term, Key::Char('j')).await?;
            }
            visible(term, "› Nightly  yesterday's copy").await?;
            press(term, Key::Enter).await?;
            visible(term, "Answered mirror-choice").await?;
            let (status, answer) = fixture.record("mirror-choice")?;
            assert_eq!(status, "answered");
            assert_eq!(answer.as_deref(), Some("Nightly: yesterday's copy"));
            Ok(())
        })
    })
    .await
}

/// `X` lists the stale records, names the count, and only on confirmation
/// archives them. Archived rows stay in the store.
#[tokio::test]
async fn test_e2e_manager_decisions_stale_bulk_archive() -> E2eResult<()> {
    run("stale-archive", |h, term, fixture| {
        Box::pin(async move {
            visible(term, "STALE 1").await?;
            // Cancel first: nothing changes.
            press(term, Key::Char('X')).await?;
            visible(term, "Archive 1 stale decision?").await?;
            visible(term, "Who holds integration-target custody?").await?;
            shot(h, term, "decisions-archive-confirm").await?;
            press(term, Key::Escape).await?;
            visible(term, "Nothing archived.").await?;
            assert_eq!(
                fixture.archived("D24-integration-target-custody")?,
                Some(("pending".into(), false))
            );
            // Confirm.
            press(term, Key::Char('X')).await?;
            visible(term, "Archive 1 stale decision?").await?;
            press(term, Key::Enter).await?;
            visible(term, "Archived 1 stale decision").await?;
            shot(h, term, "decisions-archive-done").await?;
            assert_eq!(
                fixture.archived("D24-integration-target-custody")?,
                Some(("archived".into(), true))
            );
            // The live records were left alone.
            assert_eq!(fixture.record("census-access")?.0, "pending");
            Ok(())
        })
    })
    .await
}
