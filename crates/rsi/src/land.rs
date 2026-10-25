//! `<Space>gl` / `:land` (#2055): land the focused session's branch through the
//! daemon's merge queue instead of a manual `git merge` and push.
//!
//! The first press resolves the session's branch tip with a dry run and shows
//! the commit and where it stands (unmerged, already on rolling, dirty). It arms
//! that exact commit for [`CONFIRM_WINDOW`]; the same action on the same session
//! within the window confirms and enqueues what was shown. Several sessions
//! each enqueue as their own entry, so the queue batches them. Every queued
//! entry is then watched through the operator `GetRollingQueue` read and its
//! outcome is announced when it settles.

use crate::app::App;
use rsi_common::rolling_queue::{
    EnqueueRollingQueueSourceParams, RollingQueueEntryState, RollingQueueEntryV1,
};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a shown commit stays armed for confirmation.
pub(crate) const CONFIRM_WINDOW: Duration = Duration::from_secs(30);
const WATCH_INTERVAL: Duration = Duration::from_secs(5);
const WATCH_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct Armed {
    pub(crate) session_id: Uuid,
    pub(crate) commit: String,
    pub(crate) at: Instant,
}

impl Armed {
    /// Whether this arming confirms `session_id` at `now`.
    pub(crate) fn confirms(&self, session_id: Uuid, now: Instant) -> bool {
        self.session_id == session_id && now.saturating_duration_since(self.at) < CONFIRM_WINDOW
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Watched {
    pub(crate) entry_id: Uuid,
    pub(crate) label: String,
}

type QueueRead = Result<rsi_common::rolling_queue::RollingQueueResponse, String>;

/// Operator landing state held by the `App`.
#[derive(Default)]
pub(crate) struct LandState {
    pub(crate) armed: Option<Armed>,
    pub(crate) watching: Vec<Watched>,
    poll: Option<tokio::task::JoinHandle<QueueRead>>,
    next_poll_at: Option<Instant>,
}

impl Drop for LandState {
    fn drop(&mut self) {
        if let Some(handle) = self.poll.take() {
            handle.abort();
        }
    }
}

fn params(
    session_id: Uuid,
    commit: Option<&str>,
    key: Option<String>,
    dry_run: bool,
) -> EnqueueRollingQueueSourceParams {
    crate::portable_cli::land_params(
        &crate::portable_cli::LandTarget::Session(session_id),
        commit,
        key,
        dry_run,
    )
}

/// The `<Space>gl` / `:land` action for the focused session.
pub(crate) async fn land_focused_session(app: &mut App) {
    let Some(session_id) = app.selected_session_id() else {
        app.notify("No session selected");
        return;
    };
    let now = Instant::now();
    match app.land.armed.take() {
        Some(armed) if armed.confirms(session_id, now) => confirm(app, armed).await,
        _ => preview(app, session_id, now).await,
    }
    app.mark_dirty();
}

async fn preview(app: &mut App, session_id: Uuid, now: Instant) {
    match app
        .client
        .enqueue_rolling_queue_source(&params(session_id, None, None, true))
        .await
    {
        Ok(resolved) => {
            let summary = resolved.source.summary();
            if let Some(blocker) = resolved.source.blocker() {
                app.notify_error(format!("{summary}: {blocker}"));
                return;
            }
            app.land.armed = Some(Armed {
                session_id,
                commit: resolved.source.commit,
                at: now,
            });
            app.notify(format!(
                "Land {summary}? Press <Space>gl (or :land) again within {}s to queue it.",
                CONFIRM_WINDOW.as_secs()
            ));
        }
        Err(error) => app.notify_error(format!("Land: {error}")),
    }
}

async fn confirm(app: &mut App, armed: Armed) {
    let key = format!("operator-land-{}", Uuid::new_v4());
    match app
        .client
        .enqueue_rolling_queue_source(&params(
            armed.session_id,
            Some(&armed.commit),
            Some(key),
            false,
        ))
        .await
    {
        Ok(queued) => {
            let label = queued.source.summary();
            if let Some(entry) = queued.entry {
                app.notify_success(format!("Queued: {}", entry.status_line()));
                app.land.next_poll_at = Some(Instant::now() + WATCH_INTERVAL);
                app.land.watching.push(Watched {
                    entry_id: entry.id,
                    label,
                });
            }
        }
        Err(error) => app.notify_error(format!("Land: {error}")),
    }
}

/// The outcome announcement for an entry that settled.
pub(crate) fn settled_message(label: &str, entry: &RollingQueueEntryV1) -> String {
    let verdict = match entry.state {
        RollingQueueEntryState::Published => "landed",
        _ => "NOT landed",
    };
    format!("{verdict}: {label} - {}", entry.status_line())
}

impl App {
    /// Complete one bounded queue read and start the next while landings are
    /// being watched; announces each entry once, when it settles.
    pub(crate) async fn poll_land_watch(&mut self) {
        if self
            .land
            .poll
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(handle) = self.land.poll.take()
        {
            if let Ok(Ok(queue)) = handle.await {
                self.settle_watched(&queue.entries);
            }
        }
        if self.land.watching.is_empty()
            || !self.poll.connected
            || self.land.poll.is_some()
            || self
                .land
                .next_poll_at
                .is_some_and(|due| Instant::now() < due)
        {
            return;
        }
        self.land.next_poll_at = Some(Instant::now() + WATCH_INTERVAL);
        let socket_path = self.client.socket_path().to_path_buf();
        // #2068: read the watched entries by id, not the newest page.
        let watched_ids: Vec<Uuid> = self
            .land
            .watching
            .iter()
            .map(|watched| watched.entry_id)
            .take(rsi_common::rolling_queue::ROLLING_QUEUE_WATCH_IDS_MAX)
            .collect();
        self.land.poll = Some(tokio::spawn(async move {
            tokio::time::timeout(WATCH_READ_TIMEOUT, async {
                let mut client = crate::client::DaemonClient::new(socket_path);
                client.connect().await.map_err(|error| error.to_string())?;
                client
                    .get_rolling_queue_entries(&watched_ids)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|_| "queue read timed out".to_string())?
        }));
    }

    pub(crate) fn settle_watched(&mut self, entries: &[RollingQueueEntryV1]) {
        let mut settled = Vec::new();
        self.land.watching.retain(|watched| {
            match entries.iter().find(|entry| entry.id == watched.entry_id) {
                Some(entry) if entry.state.is_terminal() => {
                    settled.push((watched.label.clone(), entry.clone()));
                    false
                }
                _ => true,
            }
        });
        for (label, entry) in settled {
            let message = settled_message(&label, &entry);
            if entry.state == RollingQueueEntryState::Published {
                self.notify_success(message.clone());
            } else {
                self.notify_error(message.clone());
            }
            self.push_notification(
                crate::types::NotificationKind::Info,
                crate::types::NotificationPriority::High,
                message,
                Some(entry.source_session_id),
            );
            self.mark_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::rolling_queue::{RollingQueueBinding, RollingQueueOutcome};

    fn entry(state: RollingQueueEntryState, landed: Option<&str>) -> RollingQueueEntryV1 {
        RollingQueueEntryV1 {
            issue: None,
            id: Uuid::new_v4(),
            sequence: 7,
            source_commit: "a".repeat(40),
            source_session_id: Uuid::new_v4(),
            owner_epic_id: None,
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: None,
            hot_files: Vec::new(),
            derived_test_filters: Vec::new(),
            test_filters: Vec::new(),
            fixes_tests: Vec::new(),
            filters_state: None,
            expedite_reason: None,
            state,
            outcome: landed.map(|sha| RollingQueueOutcome {
                landed_sha: Some(sha.into()),
                ..RollingQueueOutcome::default()
            }),
            enqueued_at: "2026-10-12T00:00:00.000000000Z".into(),
            finished_at: None,
        }
    }

    #[test]
    fn an_arming_confirms_only_the_same_session_inside_the_window() {
        let session = Uuid::new_v4();
        let at = Instant::now();
        let armed = Armed {
            session_id: session,
            commit: "a".repeat(40),
            at,
        };
        assert!(armed.confirms(session, at));
        assert!(!armed.confirms(Uuid::new_v4(), at));
        assert!(!armed.confirms(session, at + CONFIRM_WINDOW));
    }

    #[test]
    fn a_settled_entry_is_announced_with_its_outcome() {
        let landed = entry(RollingQueueEntryState::Published, Some(&"b".repeat(40)));
        assert_eq!(
            settled_message("rsi/x @ aaaaaaaaa", &landed),
            "landed: rsi/x @ aaaaaaaaa - queue #7 aaaaaaaaa: published (landed bbbbbbbbb)"
        );
        let refused = entry(RollingQueueEntryState::Refused, None);
        assert!(settled_message("rsi/x", &refused).starts_with("NOT landed: rsi/x"));
    }

    #[tokio::test]
    async fn only_settled_watched_entries_are_announced_and_dropped() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let done = entry(RollingQueueEntryState::Published, Some(&"b".repeat(40)));
        let running = entry(RollingQueueEntryState::Gating, None);
        app.land.watching = vec![
            Watched {
                entry_id: done.id,
                label: "done".into(),
            },
            Watched {
                entry_id: running.id,
                label: "running".into(),
            },
        ];
        app.settle_watched(&[done, running.clone()]);
        assert_eq!(app.land.watching.len(), 1);
        assert_eq!(app.land.watching[0].entry_id, running.id);
    }

    /// A fake daemon that answers `EnqueueRollingQueueSource` and records every
    /// request it receives.
    async fn serve(
        listener: tokio::net::UnixListener,
        session_id: Uuid,
        requests: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let commit = "c".repeat(40);
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let params = request["params"].clone();
                let dry_run = params["dry_run"].as_bool().unwrap_or(false);
                requests.lock().unwrap().push(request.clone());
                let source = serde_json::json!({
                    "session_id": session_id,
                    "branch": "rsi/fake",
                    "commit": commit,
                    "ahead": 2,
                    "on_rolling": false,
                    "dirty": false,
                    "title": null,
                });
                let entry = (!dry_run).then(|| {
                    serde_json::to_value(entry(RollingQueueEntryState::Queued, None)).unwrap()
                });
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {"source": source, "entry": entry, "replayed": false},
                });
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn the_first_press_shows_the_commit_and_the_second_enqueues_exactly_it() {
        let dir = crate::test_support::socket_tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.selected_session_id().expect("a selected session");
        app.client = crate::client::DaemonClient::new(socket);
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = tokio::spawn(serve(listener, session_id, requests.clone()));
        app.client.connect().await.expect("fake daemon connection");

        land_focused_session(&mut app).await;
        {
            let seen = requests.lock().unwrap();
            assert_eq!(seen.len(), 1, "the first press only previews");
            assert_eq!(seen[0]["method"], "EnqueueRollingQueueSource");
            assert_eq!(seen[0]["params"]["dry_run"], true);
        }
        let armed = app.land.armed.clone().expect("the shown commit is armed");
        assert_eq!(armed.commit, "c".repeat(40));
        assert!(app.land.watching.is_empty());

        land_focused_session(&mut app).await;
        {
            let seen = requests.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[1]["params"]["dry_run"], false);
            assert_eq!(seen[1]["params"]["commit"], "c".repeat(40));
            assert!(
                seen[1]["params"]["idempotency_key"]
                    .as_str()
                    .is_some_and(|key| key.starts_with("operator-land-"))
            );
        }
        assert!(app.land.armed.is_none());
        assert_eq!(app.land.watching.len(), 1, "the queued entry is watched");
        server.abort();
    }
}
