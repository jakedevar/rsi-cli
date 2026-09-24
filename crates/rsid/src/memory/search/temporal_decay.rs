use std::collections::HashMap;

use crate::memory::store::MemoryStore;

use super::ScoredChunk;

/// Milliseconds in a day.
const DAY_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

/// Calculate the temporal decay multiplier for a given age and half-life.
///
/// Formula: `exp(-lambda * age_days)` where `lambda = ln(2) / half_life_days`
///
/// Returns 1.0 (no decay) if half_life_days is 0 or negative.
pub fn calculate_temporal_decay_multiplier(age_days: f64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let lambda = f64::ln(2.0) / half_life_days;
    let clamped_age = age_days.max(0.0);
    if lambda <= 0.0 || !clamped_age.is_finite() {
        return 1.0;
    }
    (-lambda * clamped_age).exp()
}

/// Extract a date from a dated memory file path.
///
/// Matches paths like `memory/2026-02-28.md`. Returns the date as a Unix
/// timestamp in milliseconds (midnight UTC of the extracted date).
fn parse_memory_date_from_path(file_path: &str) -> Option<i64> {
    let normalized = file_path.replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);

    // Look for "memory/YYYY-MM-DD.md" pattern
    let memory_prefix = "memory/";
    let idx = normalized.find(memory_prefix)?;
    let after = &normalized[idx + memory_prefix.len()..];

    // Must be exactly "YYYY-MM-DD.md" (13 chars)
    if after.len() < 13 {
        return None;
    }
    let date_part = &after[..13];
    if !date_part.ends_with(".md") {
        return None;
    }
    let date_str = &date_part[..10]; // "YYYY-MM-DD"

    // Parse components
    if date_str.len() != 10 || date_str.as_bytes()[4] != b'-' || date_str.as_bytes()[7] != b'-' {
        return None;
    }
    let year: i32 = date_str[..4].parse().ok()?;
    let month: u32 = date_str[5..7].parse().ok()?;
    let day: u32 = date_str[8..10].parse().ok()?;

    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let datetime = date.and_hms_opt(0, 0, 0)?;
    Some(datetime.and_utc().timestamp_millis())
}

/// Check if a memory file path is "evergreen" (should never have temporal decay applied).
fn is_evergreen_memory_path(file_path: &str) -> bool {
    let normalized = file_path.replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);

    if normalized == "MEMORY.md" || normalized == "memory.md" {
        return true;
    }
    if !normalized.starts_with("memory/") {
        return false;
    }
    // Dated files under memory/ are not evergreen
    if parse_memory_date_from_path(normalized).is_some() {
        return false;
    }
    // Non-dated files under memory/ are evergreen
    true
}

/// Apply temporal decay to search results based on file age.
///
/// For each result:
/// 1. If the path is evergreen, skip (no decay).
/// 2. Try to extract a date from the path.
/// 3. Fall back to file mtime from store.
/// 4. Apply exponential decay based on age.
pub fn apply_temporal_decay(
    results: &mut [ScoredChunk],
    half_life_days: f64,
    now_ms: i64,
    store: &MemoryStore,
) {
    if half_life_days <= 0.0 {
        return;
    }

    let mut ts_cache: HashMap<String, Option<i64>> = HashMap::new();

    for result in results.iter_mut() {
        let cache_key = format!("{}:{}", result.source, result.path);

        let timestamp = ts_cache.entry(cache_key).or_insert_with(|| {
            if is_evergreen_memory_path(&result.path) {
                return None;
            }
            if let Some(ts) = parse_memory_date_from_path(&result.path) {
                return Some(ts);
            }
            store.get_file_mtime(&result.path)
        });

        if let Some(ts_ms) = timestamp {
            let age_ms = (now_ms - *ts_ms).max(0) as f64;
            let age_days = age_ms / DAY_MS;
            result.score *= calculate_temporal_decay_multiplier(age_days, half_life_days);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::MemorySource;

    fn make_chunk(path: &str, source: MemorySource, score: f64) -> ScoredChunk {
        ScoredChunk {
            id: format!("{}:{}:1:hash", path, source),
            path: path.to_string(),
            source,
            start_line: 1,
            end_line: 10,
            text: "test text".to_string(),
            score,
            vector_score: score,
            text_score: 0.0,
        }
    }

    // --- calculate_temporal_decay_multiplier tests ---

    #[test]
    fn test_calculate_decay_zero_age() {
        let m = calculate_temporal_decay_multiplier(0.0, 30.0);
        assert!((m - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_decay_at_half_life() {
        let m = calculate_temporal_decay_multiplier(30.0, 30.0);
        assert!((m - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_decay_at_double_half_life() {
        let m = calculate_temporal_decay_multiplier(60.0, 30.0);
        assert!((m - 0.25).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_decay_zero_half_life() {
        let m = calculate_temporal_decay_multiplier(10.0, 0.0);
        assert!((m - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_decay_negative_half_life() {
        let m = calculate_temporal_decay_multiplier(10.0, -10.0);
        assert!((m - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_decay_negative_age() {
        let m = calculate_temporal_decay_multiplier(-5.0, 30.0);
        assert!((m - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_decay_large_age() {
        let m = calculate_temporal_decay_multiplier(365.0, 30.0);
        assert!(m < 0.001);
        assert!(m > 0.0);
    }

    // --- parse_memory_date_from_path tests ---

    #[test]
    fn test_parse_date_from_path_valid() {
        let ts = parse_memory_date_from_path("memory/2026-02-28.md").unwrap();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 2, 28).unwrap();
        let expected = date
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        assert_eq!(ts, expected);
    }

    #[test]
    fn test_parse_date_from_path_nested() {
        let ts = parse_memory_date_from_path("./memory/2026-02-28.md");
        assert!(ts.is_some());
    }

    #[test]
    fn test_parse_date_from_path_not_memory() {
        assert!(parse_memory_date_from_path("notes/2026-02-28.md").is_none());
    }

    #[test]
    fn test_parse_date_from_path_invalid_date() {
        assert!(parse_memory_date_from_path("memory/2026-13-32.md").is_none());
    }

    #[test]
    fn test_parse_date_from_path_no_date() {
        assert!(parse_memory_date_from_path("memory/architecture.md").is_none());
    }

    #[test]
    fn test_parse_date_from_path_root_memory() {
        assert!(parse_memory_date_from_path("MEMORY.md").is_none());
    }

    #[test]
    fn test_parse_date_from_path_backslash() {
        let ts = parse_memory_date_from_path("memory\\2026-02-28.md");
        assert!(ts.is_some());
    }

    // --- is_evergreen_memory_path tests ---

    #[test]
    fn test_is_evergreen_root_memory() {
        assert!(is_evergreen_memory_path("MEMORY.md"));
    }

    #[test]
    fn test_is_evergreen_alt_memory() {
        assert!(is_evergreen_memory_path("memory.md"));
    }

    #[test]
    fn test_is_evergreen_undated_subfile() {
        assert!(is_evergreen_memory_path("memory/architecture.md"));
    }

    #[test]
    fn test_is_evergreen_dated_file() {
        assert!(!is_evergreen_memory_path("memory/2026-02-28.md"));
    }

    #[test]
    fn test_is_evergreen_session() {
        assert!(!is_evergreen_memory_path("sessions/abc-123"));
    }

    #[test]
    fn test_is_evergreen_random_path() {
        assert!(!is_evergreen_memory_path("src/main.rs"));
    }

    // --- apply_temporal_decay tests ---

    #[test]
    fn test_apply_decay_disabled() {
        let store = MemoryStore::open_in_memory().unwrap();
        let mut chunks = vec![make_chunk(
            "memory/2026-01-01.md",
            MemorySource::Memory,
            1.0,
        )];
        let original_score = chunks[0].score;
        apply_temporal_decay(&mut chunks, 0.0, 1_000_000, &store);
        assert!((chunks[0].score - original_score).abs() < f64::EPSILON);
    }

    #[test]
    fn test_apply_decay_evergreen_exempt() {
        let store = MemoryStore::open_in_memory().unwrap();
        let mut chunks = vec![make_chunk("MEMORY.md", MemorySource::Memory, 1.0)];
        let original_score = chunks[0].score;
        // Use a very old "now" to ensure decay would be significant
        let now_ms = chrono::NaiveDate::from_ymd_opt(2030, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        apply_temporal_decay(&mut chunks, 30.0, now_ms, &store);
        assert!((chunks[0].score - original_score).abs() < f64::EPSILON);
    }

    #[test]
    fn test_apply_decay_dated_file() {
        let store = MemoryStore::open_in_memory().unwrap();
        let mut chunks = vec![make_chunk(
            "memory/2026-01-01.md",
            MemorySource::Memory,
            1.0,
        )];
        // ~58 days later
        let now_ms = chrono::NaiveDate::from_ymd_opt(2026, 2, 28)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        apply_temporal_decay(&mut chunks, 30.0, now_ms, &store);
        // Score should be reduced (about 58 days with 30-day half-life)
        assert!(chunks[0].score < 1.0);
        assert!(chunks[0].score > 0.0);
        // Approximately: exp(-ln(2)/30 * 58) ≈ 0.265
        assert!((chunks[0].score - 0.265).abs() < 0.01);
    }

    #[test]
    fn test_apply_decay_undated_memory_file() {
        let store = MemoryStore::open_in_memory().unwrap();
        let mut chunks = vec![make_chunk(
            "memory/architecture.md",
            MemorySource::Memory,
            1.0,
        )];
        let now_ms = chrono::Utc::now().timestamp_millis();
        apply_temporal_decay(&mut chunks, 30.0, now_ms, &store);
        // Evergreen = no decay
        assert!((chunks[0].score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_apply_decay_session_chunk() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Insert a file entry with known mtime
        let mtime_ms = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        store
            .conn()
            .execute(
                "INSERT INTO files (path, source, hash, mtime, size) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["sessions/abc-123", "sessions", "hash", mtime_ms, 100],
            )
            .unwrap();

        let mut chunks = vec![make_chunk("sessions/abc-123", MemorySource::Sessions, 1.0)];
        let now_ms = chrono::NaiveDate::from_ymd_opt(2026, 2, 28)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        apply_temporal_decay(&mut chunks, 30.0, now_ms, &store);
        assert!(chunks[0].score < 1.0);
    }

    #[test]
    fn test_apply_decay_no_timestamp_found() {
        let store = MemoryStore::open_in_memory().unwrap();
        let mut chunks = vec![make_chunk("sessions/unknown", MemorySource::Sessions, 1.0)];
        let now_ms = chrono::Utc::now().timestamp_millis();
        apply_temporal_decay(&mut chunks, 30.0, now_ms, &store);
        // No timestamp found -> no decay
        assert!((chunks[0].score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_apply_decay_caches_timestamps() {
        let store = MemoryStore::open_in_memory().unwrap();
        let mut chunks = vec![
            make_chunk("memory/2026-01-01.md", MemorySource::Memory, 1.0),
            make_chunk("memory/2026-01-01.md", MemorySource::Memory, 0.8),
        ];
        // Give them different IDs so they're distinct chunks
        chunks[1].id = "memory/2026-01-01.md:memory:11:hash2".to_string();
        chunks[1].start_line = 11;
        chunks[1].end_line = 20;

        let now_ms = chrono::NaiveDate::from_ymd_opt(2026, 2, 28)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        apply_temporal_decay(&mut chunks, 30.0, now_ms, &store);

        // Both should be decayed by the same multiplier
        let multiplier = chunks[0].score / 1.0;
        let expected_second = 0.8 * multiplier;
        assert!((chunks[1].score - expected_second).abs() < 1e-10);
    }
}
