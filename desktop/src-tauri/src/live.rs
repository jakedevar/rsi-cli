//! Live updates: one dedicated daemon connection in push (`Subscribe`) mode.
//!
//! The task opens its own connection, sends `Subscribe`, reads the ack and
//! then forwards each bus event line to the webview as a small, bounded
//! [`LiveEvent`] (`event_type` plus an optional session id; the daemon payload
//! itself is never forwarded). When the daemon restarts or the stream drops it
//! reconnects with exponential backoff and reports link transitions so the UI
//! can fall back to fast polling while the link is down.
//!
//! The loop is generic over an emit callback so it can be tested against a
//! fake `UnixListener` without a Tauri runtime.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Tauri event carrying a [`LiveEvent`].
pub const EVENT_NAME: &str = "daemon-event";
/// Tauri event carrying a [`LinkEvent`].
pub const LINK_EVENT_NAME: &str = "daemon-link";

/// Upper bound on one pushed line. Larger lines are skipped, not parsed, and
/// surface as an [`OVERSIZE`] event so the UI refreshes instead.
pub const MAX_LINE_BYTES: usize = 256 * 1024;
/// Synthetic event type for a line that exceeded [`MAX_LINE_BYTES`].
pub const OVERSIZE: &str = "oversize";
const MAX_EVENT_TYPE_LEN: usize = 64;
const ACK_DEADLINE: Duration = Duration::from_secs(10);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);

/// Bus event types the desktop cares about (server-side `Subscribe` filter).
/// `subscription_reset` is always delivered by the daemon after a lag.
pub const SUBSCRIBED_EVENT_TYPES: &[&str] = &[
    "session_created",
    "session_status_changed",
    "conversation_event",
    "session_deleted",
    "session_archived",
    "session_unarchived",
    "session_metadata_changed",
    "session_summary_updated",
    "session_question_raised",
    "session_retrying",
    "session_stalled",
    "session_classified",
    "session_reconciled",
    "child_spawned",
];

static LINK_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record the current link state (read back by [`live_link`]).
pub fn set_link(up: bool) {
    LINK_UP.store(up, std::sync::atomic::Ordering::Relaxed);
}

/// Current push-link state. Lets the webview learn the state it missed if the
/// first `daemon-link` event fired before its listener was registered. Local
/// state only: this does not touch the daemon.
#[tauri::command]
pub fn live_link() -> bool {
    LINK_UP.load(std::sync::atomic::Ordering::Relaxed)
}

/// What the webview receives on `daemon-event`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveEvent {
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// What the webview receives on `daemon-link`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LinkEvent {
    pub up: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveMsg {
    Link(bool),
    Event(LiveEvent),
}

/// Exponential reconnect delay: `base`, `2*base`, ... capped at `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    pub const fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            attempt: 0,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let factor = 1u32.checked_shl(self.attempt).unwrap_or(u32::MAX);
        self.attempt = self.attempt.saturating_add(1);
        self.base.saturating_mul(factor).min(self.max)
    }

    pub const fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_millis(500), Duration::from_secs(10))
    }
}

fn valid_event_type(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= MAX_EVENT_TYPE_LEN
        && t.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

fn canonical_uuid(v: &Value) -> Option<String> {
    v.as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(|u| u.to_string())
}

/// Map one pushed bus-event line to the bounded event the webview sees.
/// Returns `None` for garbage so a bad line never reaches the UI.
pub fn map_line(line: &[u8]) -> Option<LiveEvent> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let event_type = value.get("event_type")?.as_str()?;
    if !valid_event_type(event_type) {
        return None;
    }
    let data = value.get("data");
    let session_id = data
        .and_then(|d| d.get("session_id"))
        .and_then(canonical_uuid)
        .or_else(|| {
            data.and_then(|d| d.get("session"))
                .and_then(|s| s.get("id"))
                .and_then(canonical_uuid)
        });
    Some(LiveEvent {
        event_type: event_type.to_string(),
        session_id,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Line {
    Data(Vec<u8>),
    Oversize,
    Eof,
}

/// Read one newline-terminated line, never buffering more than `max` bytes:
/// an oversize line is consumed to its newline and reported as `Oversize`.
async fn read_bounded_line<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> io::Result<Line> {
    let mut buf: Vec<u8> = Vec::new();
    let mut overflow = false;
    loop {
        let (consumed, done) = {
            let chunk = r.fill_buf().await?;
            if chunk.is_empty() {
                break;
            }
            let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => (i, true),
                None => (chunk.len(), false),
            };
            if !overflow {
                if buf.len() + take > max {
                    overflow = true;
                    buf = Vec::new();
                } else {
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            (if done { take + 1 } else { take }, done)
        };
        r.consume(consumed);
        if done {
            return Ok(if overflow {
                Line::Oversize
            } else {
                Line::Data(buf)
            });
        }
    }
    // Stream ended (possibly mid-line; the daemon always terminates lines).
    Ok(Line::Eof)
}

/// Why one subscription attempt ended.
#[derive(Debug, PartialEq, Eq)]
pub enum StreamEnd {
    /// Could not connect or the handshake failed (never became live).
    Failed(String),
    /// The stream was live and then ended or errored.
    Lost(String),
}

fn subscribe_request() -> Vec<u8> {
    let mut line = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "Subscribe",
        "params": { "event_types": SUBSCRIBED_EVENT_TYPES, "session_id": null },
    }))
    .unwrap_or_default();
    line.push(b'\n');
    line
}

/// One connect + subscribe + stream cycle. Emits `Link(true)` after the ack
/// and an `Event` per valid line; returns when the stream ends.
pub async fn stream_once<F: Fn(LiveMsg) + Sync>(socket: &Path, emit: &F) -> StreamEnd {
    let stream = match tokio::time::timeout(CONNECT_DEADLINE, UnixStream::connect(socket)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return StreamEnd::Failed(format!("connect: {e}")),
        Err(_) => return StreamEnd::Failed("connect timed out".into()),
    };
    let (read, mut write) = stream.into_split();
    if let Err(e) = write.write_all(&subscribe_request()).await {
        return StreamEnd::Failed(format!("subscribe write: {e}"));
    }
    if let Err(e) = write.flush().await {
        return StreamEnd::Failed(format!("subscribe flush: {e}"));
    }
    let mut reader = BufReader::new(read);

    match tokio::time::timeout(ACK_DEADLINE, read_bounded_line(&mut reader, MAX_LINE_BYTES)).await {
        Err(_) => return StreamEnd::Failed("subscribe ack timed out".into()),
        Ok(Err(e)) => return StreamEnd::Failed(format!("subscribe ack: {e}")),
        Ok(Ok(Line::Eof)) => return StreamEnd::Failed("closed before subscribe ack".into()),
        Ok(Ok(Line::Oversize)) => return StreamEnd::Failed("oversize subscribe ack".into()),
        Ok(Ok(Line::Data(ack))) => {
            if let Err(e) = crate::daemon::parse_response(&ack) {
                return StreamEnd::Failed(format!("subscribe refused: {e}"));
            }
        }
    }

    emit(LiveMsg::Link(true));
    loop {
        match read_bounded_line(&mut reader, MAX_LINE_BYTES).await {
            Err(e) => return StreamEnd::Lost(format!("read: {e}")),
            Ok(Line::Eof) => return StreamEnd::Lost("stream closed".into()),
            Ok(Line::Oversize) => emit(LiveMsg::Event(LiveEvent {
                event_type: OVERSIZE.into(),
                session_id: None,
            })),
            Ok(Line::Data(bytes)) => {
                if bytes.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                if let Some(ev) = map_line(&bytes) {
                    emit(LiveMsg::Event(ev));
                }
            }
        }
    }
}

/// Reconnect forever. `Link(false)` is emitted once per up→down transition
/// (and on the first failed attempt, so the UI starts in fast-poll mode).
pub async fn run<F: Fn(LiveMsg) + Send + Sync>(socket: PathBuf, emit: F, mut backoff: Backoff) {
    let mut link: Option<bool> = None;
    loop {
        let went_live = std::sync::atomic::AtomicBool::new(false);
        let tracked = |m: LiveMsg| {
            if m == LiveMsg::Link(true) {
                went_live.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            emit(m);
        };
        stream_once(&socket, &tracked).await;
        if went_live.load(std::sync::atomic::Ordering::Relaxed) {
            link = Some(true);
            backoff.reset();
        }
        if link != Some(false) {
            link = Some(false);
            emit(LiveMsg::Link(false));
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    const SID: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";

    fn collector() -> (Arc<Mutex<Vec<LiveMsg>>>, impl Fn(LiveMsg)) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = Arc::clone(&seen);
        (seen, move |m| s2.lock().unwrap().push(m))
    }

    #[test]
    fn maps_session_id_from_data_or_session_object() {
        let a = format!(
            r#"{{"event_type":"session_status_changed","timestamp":"t","data":{{"session_id":"{}"}}}}"#,
            SID.to_uppercase()
        );
        assert_eq!(
            map_line(a.as_bytes()),
            Some(LiveEvent {
                event_type: "session_status_changed".into(),
                session_id: Some(SID.into())
            })
        );
        let b = format!(
            r#"{{"event_type":"session_created","data":{{"session":{{"id":"{SID}","query":"big"}}}}}}"#
        );
        assert_eq!(
            map_line(b.as_bytes()).unwrap().session_id.as_deref(),
            Some(SID)
        );
        let c = br#"{"event_type":"subscription_reset","data":{"missed":3}}"#;
        assert_eq!(map_line(c).unwrap().session_id, None);
    }

    #[test]
    fn payload_is_not_forwarded_and_garbage_is_dropped() {
        let ev = map_line(br#"{"event_type":"conversation_event","data":{"session_id":"nope","content":"secret"}}"#)
            .unwrap();
        assert_eq!(ev.session_id, None);
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(json, r#"{"event_type":"conversation_event"}"#);
        assert!(map_line(b"not json").is_none());
        assert!(map_line(br#"{"data":{}}"#).is_none());
        assert!(map_line(br#"{"event_type":"Bad Type!"}"#).is_none());
        assert!(map_line(br#"{"event_type":""}"#).is_none());
        let long = format!(r#"{{"event_type":"{}"}}"#, "a".repeat(65));
        assert!(map_line(long.as_bytes()).is_none());
    }

    #[test]
    fn backoff_doubles_caps_and_resets() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        let seq: Vec<u128> = (0..5).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(seq, vec![100, 200, 400, 500, 500]);
        b.reset();
        assert_eq!(b.next_delay().as_millis(), 100);
        // Very long outages never overflow.
        for _ in 0..100 {
            b.next_delay();
        }
        assert_eq!(b.next_delay().as_millis(), 500);
    }

    #[tokio::test]
    async fn bounded_reader_skips_oversize_lines() {
        let mut data = Vec::new();
        data.extend_from_slice(b"short\n");
        data.extend(std::iter::repeat_n(b'x', 100));
        data.extend_from_slice(b"\nafter\n");
        let mut r = BufReader::with_capacity(8, &data[..]);
        assert_eq!(
            read_bounded_line(&mut r, 32).await.unwrap(),
            Line::Data(b"short".to_vec())
        );
        assert_eq!(read_bounded_line(&mut r, 32).await.unwrap(), Line::Oversize);
        assert_eq!(
            read_bounded_line(&mut r, 32).await.unwrap(),
            Line::Data(b"after".to_vec())
        );
        assert_eq!(read_bounded_line(&mut r, 32).await.unwrap(), Line::Eof);
    }

    /// Accept one connection, check the Subscribe request, ack, push lines.
    async fn serve_once(listener: &UnixListener, push: &[u8], expect_filter: bool) {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut req = String::new();
        BufReader::new(r).read_line(&mut req).await.unwrap();
        let sent: Value = serde_json::from_str(&req).unwrap();
        assert_eq!(sent["method"], "Subscribe");
        assert!(sent.get("session_token").is_none());
        if expect_filter {
            assert!(
                sent["params"]["event_types"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t == "conversation_event")
            );
        }
        w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"subscribed\":true}}\n")
            .await
            .unwrap();
        w.write_all(push).await.unwrap();
        w.flush().await.unwrap();
    }

    #[tokio::test]
    async fn streams_events_after_ack_then_reports_loss() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let push = format!(
            "{{\"event_type\":\"session_status_changed\",\"data\":{{\"session_id\":\"{SID}\"}}}}\n\
             garbage\n\n\
             {{\"event_type\":\"subscription_reset\",\"data\":{{\"missed\":2}}}}\n"
        );
        let server = tokio::spawn(async move {
            serve_once(&listener, push.as_bytes(), true).await;
        });
        let (seen, emit) = collector();
        let end = stream_once(&sock, &emit).await;
        server.await.unwrap();
        assert!(matches!(end, StreamEnd::Lost(_)));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                LiveMsg::Link(true),
                LiveMsg::Event(LiveEvent {
                    event_type: "session_status_changed".into(),
                    session_id: Some(SID.into())
                }),
                LiveMsg::Event(LiveEvent {
                    event_type: "subscription_reset".into(),
                    session_id: None
                }),
            ]
        );
    }

    #[tokio::test]
    async fn refused_subscribe_is_failure_without_link_up() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = stream.into_split();
            let mut b = [0u8; 256];
            let _ = r.read(&mut b).await;
            w.write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-1,\"message\":\"no\"}}\n",
            )
            .await
            .unwrap();
        });
        let (seen, emit) = collector();
        let end = stream_once(&sock, &emit).await;
        assert!(matches!(end, StreamEnd::Failed(_)));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_reconnects_and_reports_link_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let (seen, emit) = collector();
        let backoff = Backoff::new(Duration::from_millis(10), Duration::from_millis(40));
        let task = tokio::spawn(run(sock.clone(), emit, backoff));

        // Daemon down: first failed attempt reports link down exactly once.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(*seen.lock().unwrap(), vec![LiveMsg::Link(false)]);

        // Daemon comes up: link up, event, then drop => down, reconnect => up.
        let listener = UnixListener::bind(&sock).unwrap();
        let ev = b"{\"event_type\":\"session_created\",\"data\":{}}\n";
        serve_once(&listener, ev, false).await;
        serve_once(&listener, ev, false).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();

        let got = seen.lock().unwrap().clone();
        let ev = LiveMsg::Event(LiveEvent {
            event_type: "session_created".into(),
            session_id: None,
        });
        assert_eq!(
            got,
            vec![
                LiveMsg::Link(false),
                LiveMsg::Link(true),
                ev.clone(),
                LiveMsg::Link(false),
                LiveMsg::Link(true),
                ev,
                LiveMsg::Link(false),
            ]
        );
    }
}
