use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub struct CacheCounters {
    pub hits: u64,
    pub misses: u64,
}

impl CacheCounters {
    pub fn total(&self) -> u64 {
        self.hits + self.misses
    }
}

static ENABLED: OnceLock<bool> = OnceLock::new();

#[inline]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        rsi_common::identity::env_with_legacy(
            "RSI_PROFILE",
            &["MOTHERSHIP_PROFILE", "FLYWHL_PROFILE"],
        )
        .ok()
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(true)
    })
}

thread_local! {
    static CACHE_COUNTERS: Cell<CacheCounters> = Cell::new(CacheCounters::default());
}

#[inline]
pub fn reset_cache_counters() {
    if !enabled() {
        return;
    }
    CACHE_COUNTERS.with(|cell| cell.set(CacheCounters::default()));
}

#[inline]
pub fn record_cache_hit() {
    if !enabled() {
        return;
    }
    CACHE_COUNTERS.with(|cell| {
        let mut counters = cell.get();
        counters.hits = counters.hits.saturating_add(1);
        cell.set(counters);
    });
}

#[inline]
pub fn record_cache_miss() {
    if !enabled() {
        return;
    }
    CACHE_COUNTERS.with(|cell| {
        let mut counters = cell.get();
        counters.misses = counters.misses.saturating_add(1);
        cell.set(counters);
    });
}

#[inline]
pub fn drain_cache_counters() -> CacheCounters {
    if !enabled() {
        return CacheCounters::default();
    }
    CACHE_COUNTERS.with(|cell| {
        let counters = cell.get();
        cell.set(CacheCounters::default());
        counters
    })
}

#[inline]
pub fn start_timer() -> Option<Instant> {
    if enabled() {
        Some(Instant::now())
    } else {
        None
    }
}

#[inline]
pub fn log_duration(label: &str, start: Instant, extra: impl FnOnce(Duration)) {
    let elapsed = start.elapsed();
    extra(elapsed);
    tracing::trace!(
        target = "rsi::profile",
        label,
        ms = elapsed.as_secs_f64() * 1000.0
    );
}
