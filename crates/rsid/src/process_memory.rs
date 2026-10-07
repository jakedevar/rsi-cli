//! Daemon process memory: allocator hygiene and health reporting (#960).
//!
//! Measurement (Issue #960, 2026-09-28): most of rsid's 846 MB RSS sat in
//! glibc per-thread arenas that kept the ~750 MB peaks of codegraph index
//! passes, which run on blocking threads. glibc defaults to 8 arenas per core,
//! and a free chunk inside an arena is not returned to the OS until the arena
//! is trimmed. The daemon therefore caps the arena count at startup and
//! returns free arena pages after heavy passes and on a timer. Heap counters
//! are sampled by that timer (never on the health path, which must stay
//! cheap) and reported through `GetHealthStatus`.
//!
//! Follow-up (Issue #997, 2026-09-28): after 7 h the heap held 97 MB of live
//! objects but 752 MB free inside the arenas, with RSS at 566 MB. Three
//! causes. The arena cap ran inside `#[tokio::main]`, after the runtime's
//! workers existed, and glibc caps only arenas created later; `main` now
//! applies it before building the runtime. Transparent huge pages
//! (`enabled=always`, khugepaged `max_ptes_none=511`) re-collapsed trimmed
//! ranges into resident huge pages; the daemon opts out with
//! `PR_SET_THP_DISABLE`. glibc's dynamic mmap threshold climbs toward 32 MiB
//! after large frees and moves big buffers into the arenas, where they
//! fragment; a fixed threshold keeps them on `mmap`, which returns them whole.

use rsi_common::rpc::ProcessMemoryReport;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// glibc arena cap applied at startup. The default (8 per core) let every
/// blocking thread grow and keep its own arena.
pub const ARENA_MAX: u32 = 4;

/// Fixed glibc mmap threshold. Setting it also turns off the dynamic
/// threshold (#997).
pub const MMAP_THRESHOLD_BYTES: u32 = 256 * 1024;

/// Interval of the background trim and heap sample.
pub const TRIM_INTERVAL: Duration = Duration::from_secs(60);

static APPLIED_ARENA_MAX: AtomicU32 = AtomicU32::new(0);
static APPLIED_MMAP_THRESHOLD: AtomicU32 = AtomicU32::new(0);
static THP_DISABLED: AtomicBool = AtomicBool::new(false);
static ARENA_COUNT: AtomicU32 = AtomicU32::new(0);
static RSS_AT_READY: OnceLock<u64> = OnceLock::new();
static HEAP_SAMPLED: AtomicBool = AtomicBool::new(false);
static HEAP_IN_USE: AtomicU64 = AtomicU64::new(0);
static HEAP_FREE: AtomicU64 = AtomicU64::new(0);

/// Heap counters from the allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes handed out and not yet freed (ordinary plus mmapped chunks).
    pub in_use_bytes: u64,
    /// Bytes free inside the arenas, still resident until trimmed.
    pub free_bytes: u64,
}

/// Cap glibc arenas, fix the mmap threshold and opt out of transparent huge
/// pages.
///
/// Call once in `main`, before the Tokio runtime or any other thread starts:
/// glibc applies the arena cap only to arenas created afterwards.
/// `PR_SET_THP_DISABLE` is inherited by provider processes the daemon spawns;
/// they only lose huge-page backing.
pub fn configure_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let value = i32::try_from(ARENA_MAX).unwrap_or(i32::MAX);
        // SAFETY: mallopt only changes allocator tuning parameters.
        if unsafe { nix::libc::mallopt(nix::libc::M_ARENA_MAX, value) } == 1 {
            APPLIED_ARENA_MAX.store(ARENA_MAX, Ordering::Relaxed);
        }
        let threshold = i32::try_from(MMAP_THRESHOLD_BYTES).unwrap_or(i32::MAX);
        // SAFETY: mallopt only changes allocator tuning parameters.
        if unsafe { nix::libc::mallopt(nix::libc::M_MMAP_THRESHOLD, threshold) } == 1 {
            APPLIED_MMAP_THRESHOLD.store(MMAP_THRESHOLD_BYTES, Ordering::Relaxed);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let on: nix::libc::c_ulong = 1;
        let unused: nix::libc::c_ulong = 0;
        // SAFETY: PR_SET_THP_DISABLE only sets this process's THP flag.
        if unsafe { nix::libc::prctl(nix::libc::PR_SET_THP_DISABLE, on, unused, unused, unused) }
            == 0
        {
            THP_DISABLED.store(true, Ordering::Relaxed);
        }
    }
}

/// Number of allocator arenas (the main arena plus thread arenas), counted
/// from `malloc_info`. Takes each arena lock, so call it from the trim timer.
#[must_use]
pub fn arena_count() -> Option<u32> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let mut buffer: *mut nix::libc::c_char = std::ptr::null_mut();
        let mut size: nix::libc::size_t = 0;
        // SAFETY: open_memstream stores a malloc'd buffer and its size in the
        // two out-pointers; we free the buffer after fclose.
        let stream = unsafe { nix::libc::open_memstream(&raw mut buffer, &raw mut size) };
        if stream.is_null() {
            return None;
        }
        // SAFETY: stream is a valid FILE until fclose below.
        let status = unsafe { nix::libc::malloc_info(0, stream) };
        // SAFETY: fclose flushes and finalizes buffer/size; stream is not used again.
        let closed = unsafe { nix::libc::fclose(stream) };
        if buffer.is_null() {
            return None;
        }
        // SAFETY: after fclose, buffer holds `size` initialized bytes.
        let text = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), size) }.to_vec();
        // SAFETY: buffer was allocated by open_memstream and is freed once.
        unsafe { nix::libc::free(buffer.cast()) };
        (status == 0 && closed == 0).then(|| count_arenas(&String::from_utf8_lossy(&text)))
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
}

/// Arenas in a `malloc_info` document: one `<heap nr="N">` element each.
fn count_arenas(malloc_info_xml: &str) -> u32 {
    u32::try_from(malloc_info_xml.matches("<heap nr=").count()).unwrap_or(u32::MAX)
}

/// Return free arena pages to the OS (a no-op off glibc).
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free pages; it takes each arena lock.
    unsafe {
        nix::libc::malloc_trim(0);
    }
}

/// Current allocator heap counters, when the platform reports them. This
/// walks the arenas, so call it from the trim timer, not per request.
#[must_use]
pub fn heap_stats() -> Option<HeapStats> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallinfo2 only reads allocator counters.
        let info = unsafe { nix::libc::mallinfo2() };
        let in_use = info.uordblks.saturating_add(info.hblkhd);
        Some(HeapStats {
            in_use_bytes: u64::try_from(in_use).unwrap_or(u64::MAX),
            free_bytes: u64::try_from(info.fordblks).unwrap_or(u64::MAX),
        })
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
}

/// Resident set size of this process, from `/proc/self/statm`.
#[must_use]
pub fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf only reads a system constant.
    let page_size = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) };
    pages.checked_mul(u64::try_from(page_size).ok()?)
}

/// Trim free arena pages, then record the heap counters for health reads.
pub fn trim_and_sample() {
    release_free_memory();
    if let Some(count) = arena_count() {
        ARENA_COUNT.store(count, Ordering::Relaxed);
    }
    if let Some(stats) = heap_stats() {
        HEAP_IN_USE.store(stats.in_use_bytes, Ordering::Relaxed);
        HEAP_FREE.store(stats.free_bytes, Ordering::Relaxed);
        HEAP_SAMPLED.store(true, Ordering::Release);
    }
}

/// Record the ready-time RSS and start the background trim. Needs a Tokio
/// runtime; call once when the daemon reaches `request_ready`.
pub fn on_request_ready() {
    if let Some(rss) = rss_bytes() {
        let _ = RSS_AT_READY.set(rss);
    }
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(TRIM_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // The first tick fires at once and returns startup allocations.
            ticker.tick().await;
            let _ = tokio::task::spawn_blocking(trim_and_sample).await;
        }
    });
}

const fn allocator_name() -> &'static str {
    if cfg!(feature = "dhat-heap") {
        "dhat"
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        "glibc"
    } else {
        "system"
    }
}

/// The `GetHealthStatus` memory block; `None` when RSS is unavailable.
#[must_use]
pub fn report() -> Option<ProcessMemoryReport> {
    let rss_bytes = rss_bytes()?;
    let sampled = HEAP_SAMPLED.load(Ordering::Acquire);
    let arena_max = APPLIED_ARENA_MAX.load(Ordering::Relaxed);
    let mmap_threshold = APPLIED_MMAP_THRESHOLD.load(Ordering::Relaxed);
    let arena_count = ARENA_COUNT.load(Ordering::Relaxed);
    Some(ProcessMemoryReport {
        rss_bytes,
        rss_at_ready_bytes: RSS_AT_READY.get().copied(),
        allocator: allocator_name().to_string(),
        heap_in_use_bytes: sampled.then(|| HEAP_IN_USE.load(Ordering::Relaxed)),
        heap_free_bytes: sampled.then(|| HEAP_FREE.load(Ordering::Relaxed)),
        arena_max: (arena_max > 0).then_some(arena_max),
        mmap_threshold_bytes: (mmap_threshold > 0).then_some(u64::from(mmap_threshold)),
        thp_disabled: THP_DISABLED.load(Ordering::Relaxed).then_some(true),
        arena_count: (sampled && arena_count > 0).then_some(arena_count),
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn report_carries_resident_memory_and_sampled_heap() {
        configure_allocator();
        trim_and_sample();
        let report = report().expect("linux reports RSS");
        assert!(report.rss_bytes > 0);
        #[cfg(target_env = "gnu")]
        {
            assert_eq!(report.allocator, "glibc");
            assert_eq!(report.arena_max, Some(ARENA_MAX));
            assert!(report.heap_in_use_bytes.is_some_and(|bytes| bytes > 0));
            assert!(report.heap_free_bytes.is_some());
            assert!(report.arena_count.is_some_and(|count| count >= 1));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    #[allow(clippy::expect_used)]
    fn configure_allocator_disables_huge_pages_and_fixes_the_mmap_threshold() {
        configure_allocator();
        let unused: nix::libc::c_ulong = 0;
        // SAFETY: PR_GET_THP_DISABLE only reads this process's THP flag.
        let flag = unsafe {
            nix::libc::prctl(
                nix::libc::PR_GET_THP_DISABLE,
                unused,
                unused,
                unused,
                unused,
            )
        };
        assert_eq!(flag, 1, "the daemon opts out of transparent huge pages");
        let report = report().expect("linux reports RSS");
        assert_eq!(report.thp_disabled, Some(true));
        #[cfg(target_env = "gnu")]
        assert_eq!(
            report.mmap_threshold_bytes,
            Some(u64::from(MMAP_THRESHOLD_BYTES))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn arena_count_reads_one_heap_element_per_arena() {
        let xml = "<malloc version=\"1\">\n<heap nr=\"0\">\n<sizes>\n</sizes>\n</heap>\n\
                   <heap nr=\"1\">\n</heap>\n<heap nr=\"2\">\n</heap>\n<total type=\"fast\"/>\n</malloc>\n";
        assert_eq!(count_arenas(xml), 3);
        #[cfg(target_env = "gnu")]
        assert!(arena_count().is_some_and(|count| count >= 1));
    }

    /// #997: the arena cap only binds arenas created after `mallopt`, so
    /// `main` must configure the allocator before it builds the runtime.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    #[allow(clippy::expect_used)]
    fn daemon_main_configures_the_allocator_before_building_the_runtime() {
        let main_rs = include_str!("main.rs");
        let main_start = main_rs
            .find("\nfn main() -> Result<()> {")
            .expect("sync main");
        let main_body = &main_rs[main_start..];
        let main_body = &main_body[..main_body.find("\n}\n").expect("main end")];
        let configure = main_body
            .find("process_memory::configure_allocator()")
            .expect("main configures the allocator");
        let runtime = main_body
            .find("Builder::new_multi_thread()")
            .expect("main builds the runtime");
        assert!(configure < runtime, "allocator tuning precedes the runtime");
        assert!(
            main_rs.contains("\nfn main() -> Result<()> {"),
            "main builds its runtime explicitly"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn trimming_after_a_large_free_keeps_heap_counters_consistent() {
        let block = vec![1_u8; 64 * 1024 * 1024];
        assert_eq!(block.len(), 64 * 1024 * 1024);
        drop(block);
        trim_and_sample();
        let stats = heap_stats();
        #[cfg(target_env = "gnu")]
        assert!(stats.is_some_and(|stats| stats.in_use_bytes > 0));
        #[cfg(not(target_env = "gnu"))]
        assert_eq!(stats, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn health_payload_without_memory_block_still_parses() {
        let mut value = serde_json::to_value(rsi_common::rpc::HealthStatusResponse {
            persistence_queue_depth: 0,
            persistence_queue_capacity: 1,
            last_command_duration_ms: 0,
            project_cache_size: 0,
            project_cache_hits: 0,
            project_cache_misses: 0,
            last_poll_payload_bytes: 0,
            last_poll_event_count: 0,
            provider_claude_available: false,
            provider_codex_available: false,
            provider_pioneer_available: false,
            provider_openrouter_available: false,
            provider_bedrock_available: false,
            provider_local_available: false,
            provider_antigravity_available: false,
            provider_codex_app_server_available: false,
            provider_harness_available: false,
            provider_clis_missing: Vec::new(),
            queue_pending: 0,
            queue_claimed: 0,
            queue_completed: 0,
            queue_failed: 0,
            rate_limits: Vec::new(),
            latest_daemon_restart: None,
            worker_slice_memory_pressure: None,
            process_memory: report(),
            provider_credentials: None,
            supervisor_mode: None,
        })
        .expect("serialize health");
        assert!(value["process_memory"]["rss_bytes"].as_u64().is_some());
        value
            .as_object_mut()
            .expect("health object")
            .remove("process_memory");
        let parsed: rsi_common::rpc::HealthStatusResponse =
            serde_json::from_value(value).expect("an older payload parses");
        assert_eq!(parsed.process_memory, None);
    }
}
