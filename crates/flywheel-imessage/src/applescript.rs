//! iMessage send via osascript subprocess.
//!
//! Sends messages to iMessage contacts using AppleScript.
//! Messages are sent by spawning `osascript` as a subprocess.

use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum ApplescriptError {
    #[error("osascript execution failed: {0}")]
    Execution(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Send an iMessage to a target handle (phone number or email).
pub async fn send_imessage(target: &str, text: &str) -> Result<(), ApplescriptError> {
    let escaped_text = escape_applescript(text);
    let escaped_target = escape_applescript(target);

    let script = format!(
        r#"tell application "Messages"
    set targetService to 1st service whose service type = iMessage
    set targetBuddy to buddy "{}" of targetService
    send "{}" to targetBuddy
end tell"#,
        escaped_target, escaped_text
    );

    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("osascript failed for target {}: {}", target, stderr.trim());
        return Err(ApplescriptError::Execution(stderr.to_string()));
    }

    tracing::debug!(
        "Sent iMessage to {}: {}...",
        target,
        &text[..text.len().min(50)]
    );
    Ok(())
}

/// Send an iMessage to a group chat by chat identifier.
#[allow(dead_code)]
pub async fn send_to_group(chat_identifier: &str, text: &str) -> Result<(), ApplescriptError> {
    let escaped_text = escape_applescript(text);
    let escaped_chat = escape_applescript(chat_identifier);

    let script = format!(
        r#"tell application "Messages"
    set targetChat to a reference to chat id "{}"
    send "{}" to targetChat
end tell"#,
        escaped_chat, escaped_text
    );

    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApplescriptError::Execution(stderr.to_string()));
    }

    Ok(())
}

/// Escape a string for safe embedding in AppleScript string literals.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_applescript_quotes() {
        assert_eq!(escape_applescript(r#"say "hello""#), r#"say \"hello\""#);
    }

    #[test]
    fn test_escape_applescript_backslashes() {
        assert_eq!(escape_applescript(r"path\to\file"), r"path\\to\\file");
    }

    #[test]
    fn test_escape_applescript_mixed() {
        assert_eq!(
            escape_applescript(r#"he said "hello\" world"#),
            r#"he said \"hello\\\" world"#
        );
    }

    #[test]
    fn test_escape_applescript_no_special_chars() {
        assert_eq!(escape_applescript("plain text"), "plain text");
    }

    #[test]
    fn test_escape_applescript_empty() {
        assert_eq!(escape_applescript(""), "");
    }
}
