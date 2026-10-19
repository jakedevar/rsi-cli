//! Prompt creator helpers.
//!
//! Prompts live in the daemon's `~/.rsi/prompts/*.md` store, reached only
//! through the operator prompt-file RPCs (hub #3040: `ListPromptFiles`,
//! `GetPromptFile`, `PutPromptFile`, `DeletePromptFile`; a delete moves the
//! prompt to the daemon's recoverable trash). This module converts listing
//! entries for display and names new prompts.

use crate::types::PromptMeta;
use rsi_common::prompt_files::PromptFileEntryV1;
use std::path::Path;
use std::time::SystemTime;

/// A listing entry as the prompt list shows it. `dir` is the listing's
/// prompts directory (display only; every read and write goes by name).
pub fn prompt_meta_from_entry(dir: &Path, entry: &PromptFileEntryV1) -> PromptMeta {
    let modified_at = entry
        .mtime
        .as_deref()
        .and_then(|mtime| chrono::DateTime::parse_from_rfc3339(mtime).ok())
        .map(|time| SystemTime::from(time.with_timezone(&chrono::Utc)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    PromptMeta {
        filename: entry.name.clone(),
        first_line: entry.first_line.clone(),
        modified_at,
        size_bytes: entry.size,
        path: dir.join(&entry.name),
    }
}

/// The flat name of a new prompt created at `now`:
/// `prompt-YYYYMMDD-HHMMSS.md` (UTC).
pub fn new_prompt_name(now: SystemTime) -> String {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day, hour, min, sec) = epoch_to_datetime(secs);
    format!("prompt-{year:04}{month:02}{day:02}-{hour:02}{min:02}{sec:02}.md")
}

/// Content of a new prompt.
pub const NEW_PROMPT_CONTENT: &str = "# New Prompt\n\n";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_entry_becomes_a_prompt_row() {
        let entry = PromptFileEntryV1 {
            name: "plan.md".into(),
            first_line: "Plan".into(),
            size: 42,
            mtime: Some("2026-10-18T10:15:00.000000000Z".into()),
        };
        let meta = prompt_meta_from_entry(Path::new("/data/prompts"), &entry);
        assert_eq!(meta.filename, "plan.md");
        assert_eq!(meta.first_line, "Plan");
        assert_eq!(meta.size_bytes, 42);
        assert_eq!(meta.path, Path::new("/data/prompts/plan.md"));
        let secs = meta
            .modified_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, 1_792_318_500);
    }

    #[test]
    fn a_new_prompt_name_is_a_valid_flat_md_name() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_792_318_500);
        let name = new_prompt_name(now);
        assert_eq!(name, "prompt-20261018-101500.md");
        assert_eq!(
            rsi_common::prompt_files::validate_prompt_name(&name),
            Ok(())
        );
    }
}
