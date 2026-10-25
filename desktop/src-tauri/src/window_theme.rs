//! The native window's light/dark theme follows the app's effective theme (local #283). The title bar and window
//! frame (macOS, Windows; GTK on Linux) otherwise follow the operating system, so a Light app sat in a dark title bar
//! and a dark app in a light one. The page sends `"light"`, `"dark"` or `"system"` whenever the app theme changes
//! (src/themes.ts): `"system"` hands the window back to the OS, which is what "Match system" means. Purely local:
//! nothing goes to the daemon.

use tauri::Theme;

/// The window theme a page request names: `Some(theme)` pins the window, `None` follows the OS.
pub fn parse_window_theme(raw: &str) -> Result<Option<Theme>, String> {
    match raw {
        "light" => Ok(Some(Theme::Light)),
        "dark" => Ok(Some(Theme::Dark)),
        "system" => Ok(None),
        other => Err(format!(
            "unknown window theme {:?}: expected light, dark or system",
            other.chars().take(24).collect::<String>()
        )),
    }
}

#[tauri::command]
pub fn set_window_theme(window: tauri::WebviewWindow, theme: String) -> Result<(), String> {
    window
        .set_theme(parse_window_theme(&theme)?)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_dark_and_system_map_to_the_window_theme() {
        assert_eq!(parse_window_theme("light"), Ok(Some(Theme::Light)));
        assert_eq!(parse_window_theme("dark"), Ok(Some(Theme::Dark)));
        assert_eq!(parse_window_theme("system"), Ok(None));
    }

    #[test]
    fn anything_else_is_refused_and_named_briefly() {
        let err = parse_window_theme("Light").unwrap_err();
        assert!(err.contains("expected light, dark or system"), "{err}");
        let long = parse_window_theme(&"x".repeat(500)).unwrap_err();
        assert!(long.len() < 120, "{long}");
        assert!(parse_window_theme("").is_err());
    }
}
