//! The system browser for web links (hub #3000 gap sweep G2): a right-clicked
//! link in the transcript and a to-do's URL (`<Space>do`), as the TUI's
//! best-effort opener does. No daemon method is involved. Only an absolute
//! `http` or `https` URL without whitespace or control characters is handed to
//! the platform opener (`open` on macOS, `xdg-open` on Linux and the BSDs,
//! `rundll32 url.dll,FileProtocolHandler` on Windows), as one argument with no
//! shell in between.

use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Longest URL the opener accepts.
pub const MAX_URL_BYTES: usize = 8 * 1024;

/// The URL the opener may receive, or why not.
pub fn checked_url(raw: &str) -> Result<String, String> {
    let url = raw.trim();
    if url.len() > MAX_URL_BYTES {
        return Err(format!("the link is longer than {MAX_URL_BYTES} bytes"));
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    match rest {
        Some(host) if !host.is_empty() && !host.starts_with('/') => {}
        _ => return Err("only http and https links open in the browser".into()),
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the link contains spaces or control characters".into());
    }
    Ok(url.to_string())
}

/// The platform opener and its arguments for `url`.
pub fn opener(url: &str) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "macos") {
        ("open", vec![url.to_string()])
    } else if cfg!(target_os = "windows") {
        (
            "rundll32",
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
        )
    } else {
        ("xdg-open", vec![url.to_string()])
    }
}

/// Start the platform opener with a fixed argv and no shell, and reap it when it exits.
fn spawn_opener(program: &str, args: &[String]) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    // Reap the opener when it exits so it never lingers as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[tauri::command]
pub fn open_url(url: String) -> Result<Value, String> {
    let url = checked_url(&url)?;
    let (program, args) = opener(&url);
    spawn_opener(program, &args)?;
    Ok(json!({ "opened": url }))
}

/// The System Settings pane for notifications (local #300), the one non-web address the opener may
/// receive. It is fixed here, never taken from the page; macOS 13 and later open the Notifications
/// pane, an older macOS opens System Preferences.
pub const NOTIFICATION_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.Notifications-Settings.extension";

/// Open System Settings › Notifications (the Settings row for a refused notification permission).
#[tauri::command]
pub fn open_notification_settings() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("System Settings › Notifications is a macOS pane".into());
    }
    spawn_opener("open", &[NOTIFICATION_SETTINGS_URL.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_links_reach_the_opener() {
        assert_eq!(
            checked_url(" https://example.com/a?b=c ").unwrap(),
            "https://example.com/a?b=c"
        );
        assert!(checked_url("http://localhost:8080").is_ok());
        assert!(checked_url("HTTPS://EXAMPLE.COM").is_ok());
        assert!(checked_url("file:///etc/passwd").is_err());
        assert!(checked_url("javascript:alert(1)").is_err());
        assert!(checked_url("https://").is_err());
        assert!(checked_url("https:///x").is_err());
        assert!(checked_url("https://a b").is_err());
        assert!(checked_url("https://a\u{7}").is_err());
        assert!(checked_url(&format!("https://{}", "a".repeat(MAX_URL_BYTES))).is_err());
        assert!(checked_url("-a").is_err());
    }

    #[test]
    fn the_notification_pane_address_is_fixed_and_not_a_web_link() {
        assert!(NOTIFICATION_SETTINGS_URL.starts_with("x-apple.systempreferences:"));
        assert!(
            !NOTIFICATION_SETTINGS_URL
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        );
        // The page-facing opener still refuses it: only open_notification_settings may send it.
        assert!(checked_url(NOTIFICATION_SETTINGS_URL).is_err());
    }

    #[test]
    fn the_url_is_one_argument_to_the_platform_opener() {
        let (program, args) = opener("https://e.com/?a=1&b=2");
        assert!(["open", "xdg-open", "rundll32"].contains(&program));
        assert_eq!(args.last().unwrap(), "https://e.com/?a=1&b=2");
    }
}
