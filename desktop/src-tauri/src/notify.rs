//! OS notifications for a session that needs the operator (local #294).
//!
//! The page decides when to notify (src/notify-model.ts); this module only posts the notification
//! and reports a click. Like the folder chooser (dialog.rs) the plugin is called from Rust, so the
//! webview is granted no `notification:` permission (capabilities/default.json is unchanged): the
//! page can only ask for one notification through `notify_session`, whose text and session id are
//! checked here. A click on a notification brings the window forward and emits `notification-open`
//! with the session it was about (the page listens with the `core:event:allow-listen` it already has).
//!
//! The plugin has no way to read the OS permission on desktop (it always reports "granted"), so on
//! macOS `notify_permission` reads it from UNUserNotificationCenter (local #300): granted, denied,
//! not_determined, or unknown (other systems, a build that is not an app bundle, no answer in time).
//! macOS asks the first time a notification is shown; Settings says when it was refused.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

/// The event a clicked notification emits; its payload is the session id, or null for the test one.
pub const OPEN_EVENT: &str = "notification-open";

const MAX_TITLE_CHARS: usize = 120;
const MAX_BODY_CHARS: usize = 240;
/// How many posted notifications are remembered so a click can name its session.
const REMEMBER: usize = 64;

/// The notifications posted so far: a click carries the notification id, this maps it back to a session.
#[derive(Default)]
pub struct Posted {
    inner: Mutex<PostedInner>,
    next: AtomicI32,
}

#[derive(Default)]
struct PostedInner {
    by_id: HashMap<i32, Option<String>>,
    order: VecDeque<i32>,
    last: Option<i32>,
}

impl Posted {
    /// Remember a notification about `session` and return the id to post it with.
    fn remember(&self, session: Option<String>) -> i32 {
        let id = self
            .next
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.by_id.insert(id, session).is_none() {
            inner.order.push_back(id);
        }
        while inner.order.len() > REMEMBER {
            if let Some(old) = inner.order.pop_front() {
                inner.by_id.remove(&old);
            }
        }
        inner.last = Some(id);
        id
    }

    /// The session a click was about. Some platforms do not say which notification was clicked; then it
    /// is the one posted last.
    fn session_for(&self, tapped: Option<i32>) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        tapped
            .and_then(|id| inner.by_id.get(&id))
            .or_else(|| inner.last.and_then(|id| inner.by_id.get(&id)))
            .cloned()
            .flatten()
    }
}

/// Notification text as one plain line of at most `max` characters: control characters and runs of
/// whitespace become single spaces, and a longer text ends in an ellipsis.
pub fn clean_text(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Post one OS notification. `session_id` is the session a click opens (none for the Settings test).
#[tauri::command]
pub fn notify_session(
    app: AppHandle,
    posted: State<'_, Posted>,
    session_id: Option<String>,
    title: String,
    body: String,
) -> Result<(), String> {
    let session = match session_id.as_deref() {
        Some(raw) => Some(crate::parse_uuid("session_id", raw)?),
        None => None,
    };
    let title = clean_text(&title, MAX_TITLE_CHARS);
    if title.is_empty() {
        return Err("the notification has no title".into());
    }
    let body = clean_text(&body, MAX_BODY_CHARS);
    let id = posted.remember(session);
    app.notification()
        .builder()
        .id(id)
        .title(title)
        .body(body)
        .show()
        .map_err(|e| format!("the notification was not shown: {e}"))
}

/// The notification permission as the page sees it: `granted`, `denied`, `not_determined` or `unknown`.
pub fn permission_word(status: Option<i64>) -> &'static str {
    // UNAuthorizationStatus: 0 notDetermined, 1 denied, 2 authorized, 3 provisional, 4 ephemeral.
    match status {
        Some(0) => "not_determined",
        Some(1) => "denied",
        Some(2..=4) => "granted",
        _ => "unknown",
    }
}

/// UNUserNotificationCenter's authorization status, or none when it cannot be read.
#[cfg(target_os = "macos")]
fn macos_status() -> Option<i64> {
    use std::ptr::NonNull;
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2_foundation::NSBundle;
    use objc2_user_notifications::{UNNotificationSettings, UNUserNotificationCenter};

    // currentNotificationCenter raises an Objective-C exception for a process that is not an app
    // bundle (`cargo run`, `tauri dev`), so only ask from inside RSI.app.
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier()?;
    if !bundle.bundlePath().to_string().ends_with(".app") {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // SAFETY: the system passes a live UNNotificationSettings for the duration of the call.
        let _ = tx.send(unsafe { settings.as_ref() }.authorizationStatus().0 as i64);
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getNotificationSettingsWithCompletionHandler(&handler);
    rx.recv_timeout(Duration::from_secs(2)).ok()
}

#[cfg(not(target_os = "macos"))]
fn macos_status() -> Option<i64> {
    None
}

/// Whether the system lets RSI show notifications: `granted`, `denied`, `not_determined` or `unknown`.
/// Read-only; it neither asks for permission nor posts anything.
#[tauri::command]
pub async fn notify_permission() -> String {
    let status = tauri::async_runtime::spawn_blocking(macos_status)
        .await
        .ok()
        .flatten();
    permission_word(status).to_string()
}

/// Bring the main window forward and tell the page which session the click was about.
pub fn install(app: &AppHandle) {
    let handle = app.clone();
    let hooked = app.notification().on_action(move |action| {
        if action.action_id() != "tap" {
            return;
        }
        let tapped = action.notification().map(|n| n.id());
        let session = handle.state::<Posted>().session_for(tapped);
        if let Some(window) = handle.get_webview_window("main") {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
        let _ = handle.emit(OPEN_EVENT, session);
    });
    if let Err(e) = hooked {
        eprintln!("rsi-desktop: notification clicks are not reported: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_one_short_plain_line() {
        assert_eq!(
            clean_text("  Fake worker\n asks\ta question ", 50),
            "Fake worker asks a question"
        );
        assert_eq!(clean_text("a\u{7}b\0c", 10), "a b c");
        assert_eq!(clean_text("", 10), "");
        assert_eq!(clean_text("abcdefghij", 10), "abcdefghij");
        assert_eq!(clean_text("abcdefghijk", 10), "abcdefghi…");
        assert_eq!(clean_text("日本語のテキスト", 4), "日本語…");
    }

    #[test]
    fn a_click_names_its_session_or_the_last_one() {
        let posted = Posted::default();
        let a = posted.remember(Some("aaaaaaaa-0000-4000-8000-000000000001".into()));
        let b = posted.remember(Some("bbbbbbbb-0000-4000-8000-000000000002".into()));
        assert_ne!(a, b);
        assert_eq!(
            posted.session_for(Some(a)).as_deref(),
            Some("aaaaaaaa-0000-4000-8000-000000000001")
        );
        assert_eq!(
            posted.session_for(Some(b)).as_deref(),
            Some("bbbbbbbb-0000-4000-8000-000000000002")
        );
        // A platform that cannot say which one was clicked, or an id that was forgotten: the last posted.
        assert_eq!(
            posted.session_for(None).as_deref(),
            Some("bbbbbbbb-0000-4000-8000-000000000002")
        );
        assert_eq!(
            posted.session_for(Some(9999)).as_deref(),
            Some("bbbbbbbb-0000-4000-8000-000000000002")
        );
    }

    #[test]
    fn the_authorization_status_maps_to_four_words() {
        assert_eq!(permission_word(Some(0)), "not_determined");
        assert_eq!(permission_word(Some(1)), "denied");
        assert_eq!(permission_word(Some(2)), "granted");
        // Provisional and ephemeral permissions still deliver notifications.
        assert_eq!(permission_word(Some(3)), "granted");
        assert_eq!(permission_word(Some(4)), "granted");
        // A status a newer macOS adds, or none at all (not macOS, not a bundle, no answer): unknown.
        assert_eq!(permission_word(Some(5)), "unknown");
        assert_eq!(permission_word(Some(-1)), "unknown");
        assert_eq!(permission_word(None), "unknown");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_process_that_is_not_an_app_bundle_reports_unknown_without_asking_the_system() {
        // `cargo test` runs from target/debug, not RSI.app: currentNotificationCenter would raise there.
        assert_eq!(macos_status(), None);
    }

    #[test]
    fn the_test_notification_opens_no_session() {
        let posted = Posted::default();
        assert_eq!(posted.session_for(None), None);
        let id = posted.remember(None);
        assert_eq!(posted.session_for(Some(id)), None);
    }

    #[test]
    fn only_the_newest_notifications_are_remembered() {
        let posted = Posted::default();
        let first = posted.remember(Some("aaaaaaaa-0000-4000-8000-000000000001".into()));
        for _ in 0..REMEMBER {
            posted.remember(None);
        }
        let inner = posted.inner.lock().unwrap();
        assert!(!inner.by_id.contains_key(&first));
        assert_eq!(inner.by_id.len(), REMEMBER);
        assert_eq!(inner.order.len(), REMEMBER);
    }
}
