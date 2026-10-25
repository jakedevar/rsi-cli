//! Live operator thresholds of the `when.health_anomaly` predicate (#2747):
//! `health_disk_used_pct`, `health_thermal_c`, `health_stuck_minutes` and
//! `health_provider_window_used_pct`. The
//! store mirrors each accepted write here so the scheduler reads them without
//! the runtime config (the same pattern as `thermal_guard`).

use std::sync::atomic::{AtomicU32, Ordering};

use rsi_common::health_anomaly as h;

static DISK_USED_PCT: AtomicU32 = AtomicU32::new(h::HEALTH_DISK_USED_PCT_DEFAULT);
static THERMAL_C: AtomicU32 = AtomicU32::new(h::HEALTH_THERMAL_C_DEFAULT);
static PROVIDER_WINDOW_USED_PCT: AtomicU32 =
    AtomicU32::new(h::HEALTH_PROVIDER_WINDOW_USED_PCT_DEFAULT);
static STUCK_MINUTES: AtomicU32 = AtomicU32::new(h::HEALTH_STUCK_MINUTES_DEFAULT);

/// One read of the live thresholds; 0 means that condition is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthThresholds {
    pub disk_used_pct: u32,
    pub thermal_c: u32,
    pub stuck_minutes: u32,
    pub provider_window_used_pct: u32,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            disk_used_pct: h::HEALTH_DISK_USED_PCT_DEFAULT,
            thermal_c: h::HEALTH_THERMAL_C_DEFAULT,
            stuck_minutes: h::HEALTH_STUCK_MINUTES_DEFAULT,
            provider_window_used_pct: h::HEALTH_PROVIDER_WINDOW_USED_PCT_DEFAULT,
        }
    }
}

/// The live thresholds.
#[must_use]
pub fn current() -> HealthThresholds {
    HealthThresholds {
        disk_used_pct: DISK_USED_PCT.load(Ordering::Relaxed),
        thermal_c: THERMAL_C.load(Ordering::Relaxed),
        stuck_minutes: STUCK_MINUTES.load(Ordering::Relaxed),
        provider_window_used_pct: PROVIDER_WINDOW_USED_PCT.load(Ordering::Relaxed),
    }
}

/// Publish the operator's `health_disk_used_pct`.
pub fn set_disk_used_pct(value: u32) {
    DISK_USED_PCT.store(value, Ordering::Relaxed);
}

/// Publish the operator's `health_thermal_c`.
pub fn set_thermal_c(value: u32) {
    THERMAL_C.store(value, Ordering::Relaxed);
}

/// Publish the operator's `health_stuck_minutes`.
pub fn set_stuck_minutes(value: u32) {
    STUCK_MINUTES.store(value, Ordering::Relaxed);
}

/// Publish the operator's minimum plan-window utilization for warnings.
pub fn set_provider_window_used_pct(value: u32) {
    PROVIDER_WINDOW_USED_PCT.store(value, Ordering::Relaxed);
}
