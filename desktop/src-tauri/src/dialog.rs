//! The New session dialog's Choose folder… (local #271): one native directory picker.
//!
//! The page never gets the dialog plugin. It calls this one command, which opens the OS folder
//! chooser and returns the path the person picked (or nothing when they cancelled). The command
//! reads nothing from the folder and writes nothing: the path only fills the Folder box, and rsid
//! decides what a launch may do with it. Because the plugin is called from Rust, no `dialog:`
//! permission is granted to the webview (capabilities/default.json is unchanged).

use std::path::PathBuf;

use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

/// Longest folder text accepted as the chooser's starting point.
const MAX_START_CHARS: usize = 4096;

/// The folder the chooser opens in: the text already in the Folder box, when it is a plain
/// non-empty path. Anything else opens the chooser at the system default.
pub fn start_dir(start: Option<&str>) -> Option<PathBuf> {
    let text = start?.trim();
    if text.is_empty() || text.chars().count() > MAX_START_CHARS || text.contains('\0') {
        return None;
    }
    Some(PathBuf::from(text))
}

/// Open the native folder chooser and return the chosen folder, or `None` when it was cancelled.
#[tauri::command]
pub async fn choose_folder(
    app: AppHandle,
    start: Option<String>,
) -> Result<Option<String>, String> {
    let begin = start_dir(start.as_deref());
    // The chooser blocks until it closes, so it runs off the async worker.
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = app.dialog().file().set_title("Choose a folder");
        if let Some(dir) = begin {
            dialog = dialog.set_directory(dir);
        }
        dialog.blocking_pick_folder()
    })
    .await
    .map_err(|e| format!("the folder chooser stopped: {e}"))?;
    match picked {
        None => Ok(None),
        Some(path) => path
            .into_path()
            .map(|p| Some(p.to_string_lossy().into_owned()))
            .map_err(|e| format!("the chosen folder is not a local path: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_folder_is_the_trimmed_text_or_nothing() {
        assert_eq!(
            start_dir(Some("  /work/app ")),
            Some(PathBuf::from("/work/app"))
        );
        assert_eq!(start_dir(Some("")), None);
        assert_eq!(start_dir(Some("   ")), None);
        assert_eq!(start_dir(None), None);
        assert_eq!(start_dir(Some("/a\0b")), None);
        assert_eq!(start_dir(Some(&"x".repeat(MAX_START_CHARS + 1))), None);
        assert!(start_dir(Some(&"x".repeat(MAX_START_CHARS))).is_some());
    }
}
