//! Operator setting `tool_result_storage_cap_kib` (#2373): the size bound on a
//! stored `ToolResult` body.
//!
//! `conversation_events` is 78% of `rsi.db` and its `ToolResult` bodies (raw
//! `cargo` output, RPC dumps) are 65% of the whole file, growing about 2 GB a
//! month. Every provider's events reach the table through one writer
//! (`Store::insert_event_in_transaction`), so the bound sits there and covers
//! each provider once. It is **storage only**: the event the provider or the
//! TUI sees live is untouched, and only the persisted row keeps the head and
//! the tail of an over-cap body with a marker between them.
//!
//! The store mirrors the live setting here (the way `worker_cpu_quota` and
//! `build_slice` do) because the writer is an associated function with no
//! access to the runtime config. New rows only: nothing here rewrites a row
//! that is already stored.

use serde_json::{Value, json};
use std::sync::atomic::{AtomicU32, Ordering};

/// Default cap, in KiB. Replaying the last 30 days of one real database
/// through the cap (273k results, 1424 MB) leaves 907 MB at 32 KiB
/// (33.6 MB/day from 52.7 MB/day): the bulk of the bytes is many 1-16 KiB
/// results that no cap above 16 KiB touches, so the remainder is the target
/// of body compression (#2374).
pub const TOOL_RESULT_STORAGE_CAP_KIB_DEFAULT: u32 = 32;
/// Smallest accepted non-zero value (below it the marker is most of the body).
pub const TOOL_RESULT_STORAGE_CAP_KIB_MIN: u32 = 4;
/// Largest accepted value: the Codex ingest already bounds one field at 1 MiB.
pub const TOOL_RESULT_STORAGE_CAP_KIB_MAX: u32 = 1024;
/// Key under which a truncated row's `metadata` records the cut, so a consumer
/// can tell a capped body without parsing the marker text.
pub const TRUNCATION_METADATA_KEY: &str = "storage_truncation";
/// Bytes reserved for the marker, so a stored body never exceeds the cap.
const MARKER_RESERVE_BYTES: usize = 256;

static CAP_KIB: AtomicU32 = AtomicU32::new(TOOL_RESULT_STORAGE_CAP_KIB_DEFAULT);

/// The live cap in KiB; 0 means bodies are stored whole.
#[must_use]
pub fn current_cap_kib() -> u32 {
    CAP_KIB.load(Ordering::Relaxed)
}

/// Publish the operator's value (the store calls this on every accepted write).
pub fn set_cap_kib(kib: u32) {
    CAP_KIB.store(kib, Ordering::Relaxed);
}

/// The live cap in bytes, `None` when the cap is off.
#[must_use]
pub fn current_cap_bytes() -> Option<usize> {
    let kib = current_cap_kib();
    (kib > 0).then(|| {
        usize::try_from(kib)
            .unwrap_or(usize::MAX)
            .saturating_mul(1024)
    })
}

/// Whether `kib` is an accepted setting: 0 (off) or inside the hard range.
#[must_use]
pub const fn is_valid_cap_kib(kib: u64) -> bool {
    kib == 0
        || (kib >= TOOL_RESULT_STORAGE_CAP_KIB_MIN as u64
            && kib <= TOOL_RESULT_STORAGE_CAP_KIB_MAX as u64)
}

/// The stored form of an over-cap body and the numbers the marker records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBody {
    /// Head + marker + tail; never longer than the cap it was cut to.
    pub content: String,
    /// Byte length of the body as the provider produced it.
    pub original_bytes: usize,
    /// Bytes kept from the start.
    pub head_bytes: usize,
    /// Bytes kept from the end.
    pub tail_bytes: usize,
    /// The cap, in bytes, the body was cut to.
    pub cap_bytes: usize,
}

impl StoredBody {
    /// The value recorded under [`TRUNCATION_METADATA_KEY`].
    #[must_use]
    pub fn metadata(&self) -> Value {
        json!({
            "original_bytes": self.original_bytes,
            "head_bytes": self.head_bytes,
            "tail_bytes": self.tail_bytes,
            "cap_bytes": self.cap_bytes,
        })
    }
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Cut `content` to head + marker + tail within `cap_bytes`, or `None` when it
/// already fits (the caller then stores it byte-identical). The two kept parts
/// are equal halves of what the cap leaves after the marker, each moved inward
/// to a UTF-8 boundary, so cargo's summary line at the end survives as well as
/// the command's first lines.
#[must_use]
pub fn cap_body(content: &str, cap_bytes: usize) -> Option<StoredBody> {
    if content.len() <= cap_bytes {
        return None;
    }
    let budget = cap_bytes.saturating_sub(MARKER_RESERVE_BYTES);
    let head_budget = budget / 2;
    let tail_budget = budget - head_budget;
    let head_end = floor_boundary(content, head_budget);
    let tail_start = ceil_boundary(content, content.len() - tail_budget);
    let head_bytes = head_end;
    let tail_bytes = content.len() - tail_start;
    let omitted = tail_start - head_end;
    let marker = format!(
        "\n[... {omitted} of {} bytes omitted by the rsi tool-result storage cap \
         ({cap_bytes} bytes); kept the first {head_bytes} and the last {tail_bytes} ...]\n",
        content.len()
    );
    let mut stored = String::with_capacity(head_bytes + marker.len() + tail_bytes);
    stored.push_str(&content[..head_end]);
    stored.push_str(&marker);
    stored.push_str(&content[tail_start..]);
    Some(StoredBody {
        content: stored,
        original_bytes: content.len(),
        head_bytes,
        tail_bytes,
        cap_bytes,
    })
}

/// Serializes tests (in the crates that enable `test-seam`) that change or
/// depend on the process-wide cap; `cargo test` shares it between threads.
#[cfg(any(test, feature = "test-seam"))]
pub fn test_serial_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_within_the_cap_is_left_alone() {
        assert_eq!(cap_body("", 4096), None);
        assert_eq!(cap_body("short", 4096), None);
        let exact = "x".repeat(4096);
        assert_eq!(cap_body(&exact, 4096), None);
    }

    #[test]
    fn an_over_cap_body_keeps_head_and_tail_and_records_the_original_length() {
        let body = format!(
            "{}{}{}",
            "H".repeat(10_000),
            "m".repeat(60_000),
            "T".repeat(10_000)
        );
        let cap = 8 * 1024;
        let stored = cap_body(&body, cap).expect("over the cap");
        assert!(stored.content.len() <= cap, "{}", stored.content.len());
        assert_eq!(stored.original_bytes, 80_000);
        assert_eq!(stored.cap_bytes, cap);
        assert!(stored.content.starts_with(&"H".repeat(stored.head_bytes)));
        assert!(stored.content.ends_with(&"T".repeat(stored.tail_bytes)));
        assert!(stored.head_bytes > 3_000 && stored.tail_bytes > 3_000);
        assert!(stored.content.contains("of 80000 bytes omitted"));
        assert!(
            stored
                .content
                .contains(&format!("kept the first {}", stored.head_bytes))
        );
        assert!(
            stored
                .content
                .contains(&format!("the last {}", stored.tail_bytes))
        );
        let marker_len = stored.content.len() - stored.head_bytes - stored.tail_bytes;
        assert!(marker_len < MARKER_RESERVE_BYTES, "{marker_len}");
        assert_eq!(
            stored.metadata(),
            json!({
                "original_bytes": 80_000,
                "head_bytes": stored.head_bytes,
                "tail_bytes": stored.tail_bytes,
                "cap_bytes": cap,
            })
        );
    }

    #[test]
    fn the_cut_respects_utf8_boundaries() {
        // 2-byte and 4-byte characters straddle every candidate cut point.
        for unit in ["é", "🦀", "日"] {
            let body = unit.repeat(40_000 / unit.len());
            for cap in [4096, 4097, 4098, 4099, 5000] {
                let stored = cap_body(&body, cap).expect("over the cap");
                assert!(stored.content.len() <= cap, "{unit} {cap}");
                // `String` already guarantees valid UTF-8; the kept parts are
                // whole characters of the original.
                let head = &stored.content[..stored.head_bytes];
                let tail = &stored.content[stored.content.len() - stored.tail_bytes..];
                assert!(body.starts_with(head), "{unit} {cap}");
                assert!(body.ends_with(tail), "{unit} {cap}");
            }
        }
    }

    #[test]
    fn the_marker_fits_its_reserve_for_the_largest_numbers() {
        let marker = format!(
            "\n[... {} of {} bytes omitted by the rsi tool-result storage cap \
             ({} bytes); kept the first {} and the last {} ...]\n",
            usize::MAX,
            usize::MAX,
            usize::MAX,
            usize::MAX,
            usize::MAX
        );
        assert!(marker.len() < MARKER_RESERVE_BYTES, "{}", marker.len());
    }

    #[test]
    fn the_setting_range_is_off_or_four_kib_to_one_mib() {
        for good in [0, 4, 32, 1024] {
            assert!(is_valid_cap_kib(good), "{good}");
        }
        for bad in [1, 3, 1025, u64::MAX] {
            assert!(!is_valid_cap_kib(bad), "{bad}");
        }
        assert!(is_valid_cap_kib(u64::from(
            TOOL_RESULT_STORAGE_CAP_KIB_DEFAULT
        )));
    }
}
