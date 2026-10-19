//! Tests of the TUI's cold-storage surfaces (#2384, slice S3). A fake daemon on a
//! unix socket answers the operator RPCs and records every request, so the
//! dispatch (what the TUI asks, in what order) is pinned along with the screen.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::app::app_test_helpers;
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use rsi_common::cold_storage::{ColdArchiveFileV1, ColdTotalsV1};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<Value>>>;

/// The daemon's answer to one request: a result or a JSON-RPC error message.
type Answer = Result<Value, String>;

/// Serve `answer` on `listener`, recording each request. Every connection is its
/// own task: the app's main client and a lazy tier read can both be connected.
async fn serve(
    listener: tokio::net::UnixListener,
    requests: Requests,
    answer: impl Fn(&str, &Value) -> Answer + Send + Sync + 'static,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let answer = Arc::new(answer);
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let requests = requests.clone();
        let answer = Arc::clone(&answer);
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap_or_default().to_string();
                let params = request["params"].clone();
                requests.lock().unwrap().push(request.clone());
                let body = match answer(&method, &params) {
                    Ok(result) => {
                        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                    }
                    Err(message) => json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "error": {"code": -32602, "message": message}
                    }),
                };
                writer
                    .write_all(format!("{body}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
    }
}

/// An app connected to a fake daemon that answers with `answer`.
async fn app_with_daemon(
    answer: impl Fn(&str, &Value) -> Answer + Send + Sync + 'static,
) -> (
    App,
    Requests,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let mut app = app_test_helpers::with_session_list(1);
    app.client = crate::client::DaemonClient::new(socket);
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve(listener, requests.clone(), answer));
    app.client.connect().await.expect("fake daemon connection");
    (app, requests, dir, server)
}

fn methods(requests: &Requests) -> Vec<String> {
    requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["method"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn notified(app: &App, needle: &str) -> bool {
    app.notifications
        .iter()
        .any(|notification| notification.message.contains(needle))
}

fn cold_storage(events: i64) -> ColdSessionStorageV1 {
    ColdSessionStorageV1 {
        tier: ColdTier::Cold,
        generation: Some(1),
        archived_at: Some("2026-10-14T00:00:00.000000000Z".to_string()),
        file: Some("/archive/rsi-cold-2026-10.sqlite".to_string()),
        archived_events: events,
        raw_bytes: 4096,
        compressed_bytes: 512,
        ..ColdSessionStorageV1::default()
    }
}

fn set_status(app: &mut App, status: SessionStatus) -> Uuid {
    let id = app.selected_session_id().expect("a selected session");
    app.sessions.get_mut(&id).unwrap().session.status = status;
    id
}

fn screen_text(app: &App, id: Uuid) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::session::render_session_detail(
                frame,
                Rect::new(0, 0, 140, 24),
                id,
                true,
                app,
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The badge
// ---------------------------------------------------------------------------

#[test]
fn an_archived_session_in_cold_storage_shows_the_cold_storage_badge() {
    let (mut app, id) = app_test_helpers::with_session_detail();
    app.sessions.get_mut(&id).unwrap().session.status = SessionStatus::Archived;
    app.cold_storage.record(id, cold_storage(4));
    let text = screen_text(&app, id);
    assert!(
        text.contains("[cold storage]"),
        "the header carries the badge:\n{text}"
    );
    assert!(
        text.contains(":cold-restore"),
        "the header says how to put the rows back:\n{text}"
    );
}

#[test]
fn a_deleted_session_in_cold_storage_shows_the_badge_too() {
    let (mut app, id) = app_test_helpers::with_session_detail();
    app.sessions.get_mut(&id).unwrap().session.status = SessionStatus::Deleted;
    app.cold_storage.record(id, cold_storage(2));
    assert!(screen_text(&app, id).contains("[cold storage]"));
}

#[test]
fn a_session_is_badged_only_while_it_can_be_cold() {
    assert!(can_be_cold(SessionStatus::Archived));
    assert!(can_be_cold(SessionStatus::Deleted));
    for status in [
        SessionStatus::Starting,
        SessionStatus::Running,
        SessionStatus::WaitingApproval,
        SessionStatus::Completed,
        SessionStatus::Failed,
        SessionStatus::Interrupted,
    ] {
        assert!(!can_be_cold(status), "{status:?}");
    }
    // A tier read that went stale (the session was unarchived) draws no badge.
    let (mut app, id) = app_test_helpers::with_session_detail();
    app.sessions.get_mut(&id).unwrap().session.status = SessionStatus::Archived;
    app.cold_storage.record(id, cold_storage(4));
    assert!(badge_span(&app, id).is_some());
    app.sessions.get_mut(&id).unwrap().session.status = SessionStatus::Running;
    assert!(badge_span(&app, id).is_none());
}

#[test]
fn a_restored_session_drops_the_badge() {
    let (mut app, id) = app_test_helpers::with_session_detail();
    app.sessions.get_mut(&id).unwrap().session.status = SessionStatus::Archived;
    app.cold_storage.record(id, cold_storage(4));
    assert!(badge_span(&app, id).is_some());
    app.cold_storage.record(
        id,
        ColdSessionStorageV1 {
            tier: ColdTier::Restored,
            ..cold_storage(4)
        },
    );
    assert!(badge_span(&app, id).is_none());
}

#[test]
fn a_finished_tier_read_is_recorded_and_a_failed_one_changes_nothing() {
    let mut app = app_test_helpers::with_session_list(1);
    let id = set_status(&mut app, SessionStatus::Archived);
    let revision = app.cold_storage.invalidate_reads(id);
    assert!(app.apply_cold_tier_fetch(ColdTierFetched {
        session_id: id,
        revision,
        storage: Ok(cold_storage(8)),
    }));
    assert!(app.cold_storage.is_cold(id));
    // The same answer again redraws nothing.
    assert!(!app.apply_cold_tier_fetch(ColdTierFetched {
        session_id: id,
        revision,
        storage: Ok(cold_storage(8)),
    }));
    assert!(!app.apply_cold_tier_fetch(ColdTierFetched {
        session_id: id,
        revision,
        storage: Err("timed out".into()),
    }));
    assert!(app.cold_storage.is_cold(id));
}

#[tokio::test]
async fn opening_an_archived_session_asks_the_daemon_where_its_rows_are() {
    let (mut app, requests, _dir, server) = app_with_daemon(|method, params| {
        assert_eq!(method, "GetColdSessionStorage");
        assert!(params["session_id"].is_string());
        Ok(json!({"tier": "cold", "archived_events": 6, "generation": 1}))
    })
    .await;
    let id = set_status(&mut app, SessionStatus::Archived);
    app.trigger_cold_storage_fetch_if_needed(id);
    let result = tokio::time::timeout(Duration::from_secs(5), app.cold_storage.rx.recv())
        .await
        .expect("the tier read answers")
        .expect("the channel is open");
    assert_eq!(result.session_id, id);
    assert!(app.apply_cold_tier_fetch(result));
    assert_eq!(app.cold_storage.tier(id).unwrap().archived_events, 6);
    assert_eq!(methods(&requests), vec!["GetColdSessionStorage"]);
    server.abort();
}

#[tokio::test]
async fn a_session_that_cannot_be_cold_is_never_asked_about() {
    let mut app = app_test_helpers::with_session_list(1);
    let id = set_status(&mut app, SessionStatus::Running);
    app.trigger_cold_storage_fetch_if_needed(id);
    assert!(
        app.cold_storage.inflight.is_empty(),
        "no read was started for a running session"
    );
    let id = set_status(&mut app, SessionStatus::Archived);
    app.trigger_cold_storage_fetch_if_needed(id);
    assert!(
        app.cold_storage.inflight.contains(&id),
        "an archived session starts its read"
    );
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restore_asks_the_daemon_for_the_focused_session_and_updates_the_tier() {
    let (mut app, requests, _dir, server) = app_with_daemon(|method, params| match method {
        "RestoreColdSession" => {
            assert!(params["session_id"].is_string());
            Ok(json!({"outcome": "restored", "generation": 1, "events": 4, "provenance": 4}))
        }
        "GetColdSessionStorage" => Ok(json!({"tier": "restored", "generation": 1})),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    let id = set_status(&mut app, SessionStatus::Archived);
    app.cold_storage.record(id, cold_storage(4));
    assert!(app.cold_storage.is_cold(id));

    restore_focused_session(&mut app).await;

    assert_eq!(
        methods(&requests),
        vec!["RestoreColdSession", "GetColdSessionStorage"]
    );
    assert_eq!(
        requests.lock().unwrap()[0]["params"]["session_id"],
        json!(id),
        "the focused session is the one restored"
    );
    assert!(notified(
        &app,
        "Restored from cold storage: 4 events back in the live database"
    ));
    assert_eq!(app.cold_storage.tier(id).unwrap().tier, ColdTier::Restored);
    server.abort();
}

#[tokio::test]
async fn a_held_cold_reply_cannot_undo_a_successful_restore_even_if_metadata_fails() {
    use std::sync::atomic::{AtomicBool, Ordering};

    for metadata_fails in [false, true] {
        let restored = Arc::new(AtomicBool::new(false));
        let daemon_restored = Arc::clone(&restored);
        let (mut app, requests, _dir, server) = app_with_daemon(move |method, _| match method {
            "RestoreColdSession" => {
                daemon_restored.store(true, Ordering::SeqCst);
                Ok(json!({"outcome": "restored", "generation": 1, "events": 4}))
            }
            "GetColdSessionStorage" if !daemon_restored.load(Ordering::SeqCst) => {
                Ok(serde_json::to_value(cold_storage(4)).unwrap())
            }
            "GetColdSessionStorage" if metadata_fails => Err("metadata unavailable".into()),
            "GetColdSessionStorage" => Ok(json!({"tier": "restored", "generation": 1})),
            other => Err(format!("unexpected {other}")),
        })
        .await;
        let id = set_status(&mut app, SessionStatus::Archived);
        app.cold_storage.record(id, cold_storage(4));
        app.trigger_cold_storage_fetch_if_needed(id);
        // Hold the actual lazy reply in the test until the restore finishes;
        // channel receipt, rather than elapsed time, fixes the ordering.
        let held = app.cold_storage.rx.recv().await.unwrap();
        assert_eq!(held.storage.as_ref().unwrap().tier, ColdTier::Cold);
        restore_focused_session(&mut app).await;
        let restored_storage = app.cold_storage.tier(id).unwrap().clone();
        assert_eq!(restored_storage.tier, ColdTier::Restored);
        assert_eq!(restored_storage.generation, Some(1));
        assert!(restored.load(Ordering::SeqCst));

        // Start a newer read before delivering the stale result: the old reply
        // must also leave this new read's inflight state intact.
        app.trigger_cold_storage_fetch_if_needed(id);
        let current = app.cold_storage.rx.recv().await.unwrap();
        assert!(current.revision > held.revision);
        assert!(!app.apply_cold_tier_fetch(held));
        assert!(app.cold_storage.inflight.contains(&id));
        assert_eq!(app.cold_storage.tier(id), Some(&restored_storage));
        app.apply_cold_tier_fetch(current);
        assert!(app.cold_storage.inflight.is_empty());
        assert_eq!(app.cold_storage.tier(id).unwrap().tier, ColdTier::Restored);
        assert_eq!(app.sessions[&id].session.status, SessionStatus::Archived);
        assert!(notified(
            &app,
            "Restored from cold storage: 4 events back in the live database"
        ));
        assert_eq!(
            methods(&requests),
            vec![
                "GetColdSessionStorage",
                "RestoreColdSession",
                "GetColdSessionStorage",
                "GetColdSessionStorage"
            ]
        );
        server.abort();
    }
}

#[tokio::test]
async fn restoring_a_live_session_says_it_is_already_live() {
    let (mut app, _requests, _dir, server) = app_with_daemon(|method, _| match method {
        "RestoreColdSession" => {
            Ok(json!({"outcome": "already_hot", "generation": 0, "events": 0, "provenance": 0}))
        }
        _ => Ok(json!({"tier": "live"})),
    })
    .await;
    restore_focused_session(&mut app).await;
    assert!(notified(&app, "Already in the live database"));
    server.abort();
}

#[tokio::test]
async fn a_refused_restore_shows_the_daemons_reason() {
    let (mut app, _requests, _dir, server) = app_with_daemon(|_, _| {
        Err("cold_archive_unavailable: cold_archive_file_missing (/archive/x.sqlite)".to_string())
    })
    .await;
    restore_focused_session(&mut app).await;
    assert!(notified(
        &app,
        "Restore from cold storage failed: RPC error (-32602): cold_archive_unavailable"
    ));
    server.abort();
}

#[test]
fn the_restore_message_names_each_outcome() {
    let receipt = |outcome: &str, events| ColdRestoreReceiptV1 {
        outcome: outcome.to_string(),
        generation: 1,
        events,
        provenance: 0,
    };
    assert_eq!(
        restore_message(&receipt("restored", 1)),
        "Restored from cold storage: 1 event back in the live database"
    );
    assert_eq!(
        restore_message(&receipt("already_hot", 0)),
        "Already in the live database: nothing to restore"
    );
}

// ---------------------------------------------------------------------------
// `:cold-archive` and the status rows
// ---------------------------------------------------------------------------

fn status() -> ColdArchiveStatusV1 {
    ColdArchiveStatusV1 {
        settings: rsi_common::cold_storage::ColdSettingsViewV1 {
            enabled: true,
            after_days: 14,
            dir: "/archive".into(),
        },
        live_db: rsi_common::cold_storage::ColdLiveDbV1 {
            file_bytes: Some(6_700_000_000),
            ..Default::default()
        },
        archives: vec![ColdArchiveFileV1 {
            path: "/archive/rsi-cold-2026-10.sqlite".into(),
            bytes: 600 * 1024 * 1024,
            ..Default::default()
        }],
        totals: ColdTotalsV1 {
            staged_sessions: 12,
            verified_sessions: 11,
            cold_sessions: 3,
            restored_sessions: 1,
            ..Default::default()
        },
        last_run: Some(ColdRunV1 {
            run_id: "11111111-2222-3333-4444-555555555555".into(),
            trigger: "run_now".into(),
            mode: "stage".into(),
            started_at: "2026-10-14T11:20:00.000000000Z".into(),
            outcome: "completed".into(),
            sessions_staged: 12,
            raw_bytes: 4 * 1024 * 1024,
            compressed_bytes: 600 * 1024,
            ..Default::default()
        }),
        running: false,
        ..Default::default()
    }
}

#[test]
fn the_status_line_names_the_database_the_archive_and_the_counts() {
    assert_eq!(
        status_line(&status()),
        "archive on · live DB 6.2 GiB · 1 file 600.0 MiB · 12 staged (11 verified) · 3 cold · 1 restored · idle"
    );
    let mut running = status();
    running.running = true;
    running.settings.enabled = false;
    running.live_db.file_bytes = None;
    assert!(status_line(&running).starts_with("archive off · live DB unknown"));
    assert!(status_line(&running).ends_with("running"));
}

#[test]
fn the_last_run_line_names_the_outcome_and_any_problem() {
    assert_eq!(
        last_run_line(&status()),
        "stage (run_now) · completed · 2026-10-14T11:20:00.000000000Z · 12 staged · 4.0 MiB → 600.0 KiB"
    );
    let mut failed = status();
    let run = failed.last_run.as_mut().unwrap();
    run.outcome = "failed".into();
    run.error_code = Some("cold_archive_dir_forbidden".into());
    run.error_detail = Some("the directory   resolves into a sandbox".into());
    assert!(
        last_run_line(&failed).ends_with(
            "problem: cold_archive_dir_forbidden: the directory resolves into a sandbox"
        )
    );
    let mut none = ColdArchiveStatusV1::default();
    assert_eq!(last_run_line(&none), "no pass has run yet");
    none.errors = vec!["archive /a: bad sha".into()];
    assert_eq!(
        last_run_line(&none),
        "no pass yet · problem: archive /a: bad sha"
    );
}

#[test]
fn byte_counts_read_in_the_largest_whole_unit() {
    assert_eq!(bytes_text(12), "12 B");
    assert_eq!(bytes_text(3 * 1024), "3.0 KiB");
    assert_eq!(bytes_text(5 * 1024 * 1024 + 512 * 1024), "5.5 MiB");
    assert_eq!(bytes_text(3 * 1024 * 1024 * 1024), "3.0 GiB");
    assert_eq!(bytes_text(-5), "0 B");
}

#[tokio::test]
async fn cold_archive_status_fills_the_settings_rows() {
    let (mut app, requests, _dir, server) = app_with_daemon(|method, _| match method {
        "GetColdArchiveStatus" => Ok(serde_json::to_value(status()).unwrap()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    cold_archive_command(&mut app, None).await;
    assert_eq!(methods(&requests), vec!["GetColdArchiveStatus"]);
    let row = |field: &str| {
        DaemonFeatureEntry::display_value(&app.daemon_features, field)
            .unwrap()
            .to_string()
    };
    assert_eq!(row(STATUS_FIELD), status_line(&status()));
    assert_eq!(row(LAST_RUN_FIELD), last_run_line(&status()));
    assert!(notified(&app, "Cold storage: archive on · live DB 6.2 GiB"));
    server.abort();
}

#[tokio::test]
async fn settings_keep_open_archive_problems_beside_a_completed_run_after_r_refresh() {
    use crate::modalkit_types::LcAction;
    use crate::settings_registry::SettingsSection;
    use crate::types::SettingsFocus;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut fixture = status();
    fixture.errors = vec![
        "archive /archive/older.sqlite: verification checksum mismatch".into(),
        "2 session(s) failed their last archive step".into(),
    ];
    fixture.totals.sessions_with_error = 2;
    let reply = serde_json::to_value(&fixture).unwrap();
    let (mut app, requests, _dir, server) = app_with_daemon(move |method, _| match method {
        "GetColdArchiveStatus" => Ok(reply.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.settings_state.section = SettingsSection::ColdStorage;
    app.settings_state.focus = SettingsFocus::Items;
    refresh_status(&mut app).await.unwrap();

    for after_refresh in [false, true] {
        if after_refresh {
            // Exercise the same key and daemon-feature refresh as Settings R.
            assert!(crate::settings_keys::handle_settings_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE)
            ));
            assert!(
                app.pending_lc_actions
                    .contains(&LcAction::RefreshDaemonFeatures)
            );
            DaemonFeatureEntry::set_display_text(
                &mut app.daemon_features,
                STATUS_FIELD,
                "refresh pending".into(),
            );
            crate::action_handler::daemon_config::refresh_daemon_features(&mut app).await;
        }
        let status_row =
            DaemonFeatureEntry::display_value(&app.daemon_features, STATUS_FIELD).unwrap();
        assert!(status_row.starts_with("2 open problems · 2 sessions with archive errors"));
        assert!(
            status_row
                .contains("problem: archive /archive/older.sqlite: verification checksum mismatch")
        );
        assert!(status_row.ends_with("idle"));
        let run_row =
            DaemonFeatureEntry::display_value(&app.daemon_features, LAST_RUN_FIELD).unwrap();
        assert!(run_row.starts_with(
            "stage (run_now) · completed · 2026-10-14T11:20:00.000000000Z · 12 staged"
        ));
        assert!(
            run_row
                .contains("problem: archive /archive/older.sqlite: verification checksum mismatch")
        );
        assert!(run_row.contains("2 sessions with archive errors"));
    }
    assert_eq!(
        methods(&requests)
            .iter()
            .filter(|method| method.as_str() == "GetColdArchiveStatus")
            .count(),
        2
    );
    server.abort();
}

#[tokio::test]
async fn cold_archive_run_and_verify_start_the_named_pass() {
    for (arg, mode) in [("run", "stage"), ("move", "move"), ("verify", "verify")] {
        let (mut app, requests, _dir, server) = app_with_daemon(|method, params| match method {
            "RunColdArchiveNow" => Ok(json!({"run_id": "abcdef12-0000", "mode": params["mode"]})),
            "GetColdArchiveStatus" => Ok(serde_json::to_value(status()).unwrap()),
            other => Err(format!("unexpected {other}")),
        })
        .await;
        cold_archive_command(&mut app, Some(arg)).await;
        let seen = requests.lock().unwrap().clone();
        assert_eq!(seen[0]["method"], "RunColdArchiveNow");
        assert_eq!(seen[0]["params"]["mode"], mode, "`{arg}` runs `{mode}`");
        assert_eq!(seen[1]["method"], "GetColdArchiveStatus");
        assert!(notified(
            &app,
            &format!("Cold-storage {mode} pass started (run abcdef12)")
        ));
        server.abort();
    }
}

#[tokio::test]
async fn a_refused_pass_shows_the_daemons_reason() {
    let (mut app, _requests, _dir, server) = app_with_daemon(|_, _| {
        Err("cold_archive_disabled: turn on cold_archive_enabled first".to_string())
    })
    .await;
    cold_archive_command(&mut app, Some("move")).await;
    assert!(notified(&app, "cold_archive_disabled"));
    server.abort();
}

#[tokio::test]
async fn compact_is_reported_as_scheduled_for_the_next_restart() {
    let (mut app, requests, _dir, server) = app_with_daemon(|method, params| match method {
        "RunColdArchiveNow" => Ok(json!({
            "run_id": "abcdef12-0000",
            "mode": params["mode"],
            "scheduled": true,
        })),
        "GetColdArchiveStatus" => Ok(serde_json::to_value(status()).unwrap()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    cold_archive_command(&mut app, Some("compact")).await;
    let seen = requests.lock().unwrap().clone();
    assert_eq!(seen[0]["params"]["mode"], "compact");
    assert!(notified(
        &app,
        "Cold-storage compact scheduled for the next supervised restart (run abcdef12)"
    ));
    server.abort();
}

#[tokio::test]
async fn an_unknown_cold_archive_word_names_the_usage_and_asks_nothing() {
    let (mut app, requests, _dir, server) =
        app_with_daemon(|_, _| Err("unexpected".to_string())).await;
    cold_archive_command(&mut app, Some("frobnicate")).await;
    assert!(notified(
        &app,
        "use :cold-archive [status|run|move|verify|compact]"
    ));
    assert!(methods(&requests).is_empty());
    server.abort();
}

#[test]
fn the_cold_storage_tab_lists_the_switch_the_status_and_the_run_row() {
    use crate::settings_registry::{SETTINGS, SettingId, SettingsSection};
    let ids: Vec<SettingId> = SETTINGS
        .iter()
        .filter(|spec| spec.section == SettingsSection::ColdStorage)
        .map(|spec| spec.id)
        .collect();
    assert_eq!(
        ids,
        vec![
            SettingId::ColdArchiveEnabled,
            SettingId::ColdArchiveAfterDays,
            SettingId::ColdArchiveDir,
            SettingId::ColdArchiveStatus,
            SettingId::ColdArchiveLastRun,
            SettingId::ColdArchiveRunNow,
        ]
    );
    // Every status row is backed by a daemon-feature entry the status read fills.
    let app = app_test_helpers::with_session_list(0);
    for field in [STATUS_FIELD, LAST_RUN_FIELD, RUN_NOW_FIELD] {
        assert!(
            app.daemon_features.iter().any(|entry| entry.field == field),
            "{field}"
        );
    }
    let rows =
        crate::settings_keys::daemon_feature_rows_for_section(&app, SettingsSection::ColdStorage);
    assert_eq!(rows.len(), 6);
}
