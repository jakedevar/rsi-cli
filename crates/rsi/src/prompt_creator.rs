//! Prompt creator file I/O.
//!
//! Local-only storage in `~/.rsi/prompts/`. Each prompt is a `.md` file
//! that can be opened, edited, and saved through the prompt creator view.

use crate::types::PromptMeta;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

/// Return the directory where prompt files are stored.
pub fn prompts_dir() -> PathBuf {
    rsi_common::identity::data_path("prompts", "prompts")
}

/// Scan the prompts directory for `.md` files and return metadata sorted by
/// modification time (most recent first).
pub fn load_prompt_list() -> Vec<PromptMeta> {
    let dir = prompts_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut prompts = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let first_line = match std::fs::read_to_string(&path) {
            Ok(content) => content
                .lines()
                .next()
                .unwrap_or("")
                .trim_start_matches('#')
                .trim()
                .to_string(),
            Err(_) => String::new(),
        };

        let modified_at = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let size_bytes = metadata.len();

        prompts.push(PromptMeta {
            filename,
            first_line,
            modified_at,
            size_bytes,
            path,
        });
    }

    prompts.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    prompts
}

/// Create a new prompt file with default content. Returns the path to the new file.
pub fn create_new_prompt() -> io::Result<PathBuf> {
    let dir = prompts_dir();
    std::fs::create_dir_all(&dir)?;

    // Format timestamp from SystemTime for the filename
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple timestamp formatting: YYYYMMDD-HHMMSS from epoch seconds
    // Using a basic conversion to avoid chrono dependency
    let (year, month, day, hour, min, sec) = epoch_to_datetime(secs);

    let filename = format!("prompt-{year:04}{month:02}{day:02}-{hour:02}{min:02}{sec:02}.md");
    let path = dir.join(&filename);

    std::fs::write(&path, "# New Prompt\n\n")?;
    Ok(path)
}

/// Save content to a prompt file.
pub fn save_prompt(path: &std::path::Path, content: &str) -> io::Result<()> {
    std::fs::write(path, content)
}

/// Convert epoch seconds to (year, month, day, hour, minute, second) in UTC.
fn epoch_to_datetime(epoch: u64) -> (u64, u64, u64, u64, u64, u64) {
    let sec = epoch % 60;
    let min = (epoch / 60) % 60;
    let hour = (epoch / 3600) % 24;
    let mut days = epoch / 86400;

    // Compute year
    let mut year = 1970u64;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    // Compute month
    let month_days: [u64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }

    let day = days + 1; // 1-indexed

    (year, month, day, hour, min, sec)
}

fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Format a relative time string from a SystemTime.
pub fn relative_time(time: SystemTime) -> String {
    let elapsed = time.elapsed().unwrap_or_default();
    let secs = elapsed.as_secs();

    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        let m = secs / 60;
        format!("{m}m ago")
    } else if secs < 86400 {
        let h = secs / 3600;
        format!("{h}h ago")
    } else {
        let d = secs / 86400;
        format!("{d}d ago")
    }
}

/// Format file size in human-readable form.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
