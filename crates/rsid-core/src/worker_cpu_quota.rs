//! Operator setting `worker_cpu_quota_percent` (#1992): the `CPUQuota` each
//! worker scope runs under, as a percentage of one CPU (400 = four cores).
//! 0 disables the cap. The store mirrors the live setting here so the scope
//! launchers read it without holding the runtime config; it applies to scopes
//! started after the change.

use std::sync::atomic::{AtomicU32, Ordering};

/// Default per-worker quota: four CPUs.
pub const WORKER_CPU_QUOTA_PERCENT_DEFAULT: u32 = 400;
/// Upper bound of the setting (1024 CPUs).
pub const WORKER_CPU_QUOTA_PERCENT_MAX: u32 = 102_400;

static PERCENT: AtomicU32 = AtomicU32::new(WORKER_CPU_QUOTA_PERCENT_DEFAULT);

/// The quota new worker scopes start with; 0 means uncapped.
#[must_use]
pub fn current_percent() -> u32 {
    PERCENT.load(Ordering::Relaxed)
}

/// Publish the operator's value (the store calls this on every accepted write).
pub fn set_percent(percent: u32) {
    PERCENT.store(percent, Ordering::Relaxed);
}

/// The systemd `CPUQuota=` property value for `percent`, `None` when disabled.
#[must_use]
pub fn property_value(percent: u32) -> Option<String> {
    (percent > 0).then(|| format!("{percent}%"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_disables_and_other_values_render_as_percent() {
        assert_eq!(property_value(0), None);
        assert_eq!(property_value(400).as_deref(), Some("400%"));
    }
}
