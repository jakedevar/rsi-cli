//! Host-pressure admission predicate (#2562): one answer to "is the host too
//! hot or too full to start more build-heavy work?".
//!
//! Two gates, both already operator settings:
//!
//! * thermal: the hottest CPU sensor is at or above `job_thermal_ceiling_c`
//!   (#2524, `rsid_core::thermal_guard`);
//! * disk pressure: free space on `/` is below the governor's
//!   `min_free_disk_gb` floor (#1014, `governor`).
//!
//! The build/test/landing/cloud-gate job launcher (`agent_jobs`), a manager's
//! new `create_session` and a topology node launch all ask this module, so no
//! manager polls `sensors` or `df` before a launch. New worker launches add the
//! configured per-worker target reserve through `launch_hold_now`; job admission
//! continues to use only its floor through `hold_now`. The governor's disk gate
//! compares through [`disk_below_floor`] too, so the floor has one definition.
//! A caller HOLDS on a [`PressureHold`] (queued, never refused) and starts once
//! it clears.

use rsi_common::agent_daemon_info::BuildDiskFloorV1;
use rsid_core::thermal_guard::ThermalHold;

/// `held.reason` of work held by the thermal gate.
pub const THERMAL: &str = "thermal";
/// `held.reason` of work held by the disk-pressure gate.
pub const DISK_PRESSURE: &str = "disk_pressure";
/// `held.reason` of work held because the sandbox allocator refused it for free
/// space (#2920): the sandbox floor (`sandbox_min_free_gib`, scaled down on a
/// small disk), not the governor's `min_free_disk_gb`.
pub const SANDBOX_FREE_SPACE: &str = "sandbox_free_space";

const GIB: f64 = 1_073_741_824.0;

/// Why host pressure holds a launch right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PressureHold {
    /// The host CPU is at or above `job_thermal_ceiling_c`.
    Thermal(ThermalHold),
    /// Free space on `/` is below the governor floor plus any launch reserve.
    DiskPressure {
        free_gb: f64,
        min_gb: u64,
        reserve_gb: u64,
    },
    /// The sandbox allocator refused a launch for free space (#2920): free space
    /// where sandboxes live is below the effective sandbox floor. `min_gb` is the
    /// floor rounded up. It clears on its own once space returns.
    SandboxFreeSpace { free_gb: f64, min_gb: u64 },
}

impl PressureHold {
    /// The wire `reason` (`thermal`, `disk_pressure` or `sandbox_free_space`).
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Thermal(_) => THERMAL,
            Self::DiskPressure { .. } => DISK_PRESSURE,
            Self::SandboxFreeSpace { .. } => SANDBOX_FREE_SPACE,
        }
    }

    /// One line for a log or an event.
    #[must_use]
    pub fn detail(self) -> String {
        match self {
            Self::Thermal(hot) => format!(
                "held for thermal: the host CPU is {:.1} C, at or above the {} C job_thermal_ceiling_c; the work starts once it cools",
                hot.temperature_c, hot.ceiling_c
            ),
            Self::DiskPressure {
                free_gb,
                min_gb,
                reserve_gb,
            } => {
                let reserve = if reserve_gb == 0 {
                    String::new()
                } else {
                    format!(
                        "; launch reserve {reserve_gb} GiB, needed free {} GiB",
                        min_gb.saturating_add(reserve_gb)
                    )
                };
                format!(
                    "held for disk pressure: {}{reserve}; governor_min_free_disk_gb holds the work until space is freed",
                    disk_floor_detail(free_gb, min_gb)
                )
            }
            Self::SandboxFreeSpace { free_gb, min_gb } => format!(
                "held for sandbox free space: {free_gb:.1} GiB free where sandboxes live, below the {min_gb} GiB sandbox floor; the work starts once space is freed"
            ),
        }
    }
}

/// GiB are binary despite the legacy `_gb` setting names. Include both byte
/// counts so a reading just below the floor stays visibly below it after rounding.
pub(crate) fn disk_floor_detail(free_gb: f64, min_gb: u64) -> String {
    let free_bytes = free_gb * GIB;
    let min_bytes = u128::from(min_gb) * 1_073_741_824;
    format!(
        "{free_gb:.1} GiB ({free_bytes:.0} bytes) free on /, floor {min_gb} GiB ({min_bytes} bytes)"
    )
}

/// Whether `free_gb` is below the `min_gb` floor (the governor's disk gate).
#[must_use]
pub fn disk_below_floor(free_gb: f64, min_gb: u64) -> bool {
    free_gb < min_gb as f64
}

/// Incremental reuse reduces the job's estimated growth by a quarter of its
/// own target, capped at 2 GiB and half the floor. It never increases physical
/// free space: the configured floor must already be satisfied (#569).
pub(crate) fn own_target_credit_gb(free_gb: f64, min_gb: u64, target_bytes: u64) -> f64 {
    let floor = min_gb as f64;
    let credit = (target_bytes as f64 / GIB / 4.0).min(2.0).min(floor / 2.0);
    if free_gb.is_finite() && free_gb >= floor {
        credit
    } else {
        0.0
    }
}

/// The build disk floor against one free-space reading (#163): `free_gb` is
/// `f64::MAX` where the disk cannot be read, which reports no reading and
/// stays open. A floor of 0 disables the gate.
#[must_use]
pub fn build_disk_floor(free_gb: f64, min_gb: u64) -> BuildDiskFloorV1 {
    let readable = free_gb < f64::MAX;
    BuildDiskFloorV1 {
        min_free_gb: min_gb,
        free_gb: readable.then_some(free_gb),
        open: min_gb == 0 || !readable || !disk_below_floor(free_gb, min_gb),
    }
}

/// Free GiB on `/`; `f64::MAX` where it cannot be read (nothing holds).
#[must_use]
pub fn disk_free_gb() -> f64 {
    #[cfg(unix)]
    {
        nix::sys::statvfs::statvfs("/").map_or(f64::MAX, |stats| {
            stats.blocks_available() as f64 * stats.fragment_size() as f64 / GIB
        })
    }
    #[cfg(not(unix))]
    {
        let _ = GIB;
        f64::MAX
    }
}

/// Percent of `/` in use, as `df` reports it (used over used plus available);
/// `None` where it cannot be read (#2747).
#[must_use]
pub fn disk_used_pct() -> Option<f64> {
    #[cfg(unix)]
    {
        let stats = nix::sys::statvfs::statvfs("/").ok()?;
        let fragment = stats.fragment_size() as f64;
        let used = stats.blocks().saturating_sub(stats.blocks_free()) as f64 * fragment;
        let available = stats.blocks_available() as f64 * fragment;
        let total = used + available;
        (total > 0.0).then(|| used * 100.0 / total)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The thermal hold in force on this host now (`job_thermal_ceiling_c` against
/// the hottest CPU sensor); `None` below the ceiling, when the setting is off,
/// and where no CPU sensor can be read.
#[must_use]
pub fn thermal_hold() -> Option<ThermalHold> {
    #[cfg(test)]
    if let Some(forced) = TEST_THERMAL_HOLD.with(std::cell::Cell::get) {
        return forced;
    }
    rsid_core::thermal_guard::hold_now()
}

// Test seam: the hold this thread sees (`Some(None)` = cool), `None` = real.
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_THERMAL_HOLD: std::cell::Cell<Option<Option<ThermalHold>>> =
        const { std::cell::Cell::new(None) };
}

/// The pressure hold in force now: thermal first, then disk (`min_free_disk_gb`
/// 0 disables the disk gate). `None` admits.
#[must_use]
pub fn hold_now(min_free_disk_gb: u64) -> Option<PressureHold> {
    if let Some(hot) = thermal_hold() {
        return Some(PressureHold::Thermal(hot));
    }
    if min_free_disk_gb == 0 {
        return None;
    }
    let free_gb = disk_free_gb();
    disk_hold(free_gb, min_free_disk_gb, 0)
}

/// Launches reserve one target estimate above the floor. Job admission continues
/// to call `hold_now` with no reserve. Thermal pressure keeps precedence.
#[must_use]
pub fn launch_hold_now(min_free_disk_gb: u64, reserve_gb: u64) -> Option<PressureHold> {
    if let Some(hot) = thermal_hold() {
        return Some(PressureHold::Thermal(hot));
    }
    disk_hold(disk_free_gb(), min_free_disk_gb, reserve_gb)
}

/// Compare a reading with the governor floor plus one launch's target estimate.
/// A zero reserve restores the job floor; a zero governor floor disables disk
/// admission, preserving the existing operator kill switch.
#[must_use]
pub(crate) fn disk_hold(free_gb: f64, min_gb: u64, reserve_gb: u64) -> Option<PressureHold> {
    (min_gb != 0 && disk_below_floor(free_gb, min_gb.saturating_add(reserve_gb))).then_some(
        PressureHold::DiskPressure {
            free_gb,
            min_gb,
            reserve_gb,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn own_target_reuse_requires_the_full_physical_floor() {
        let gib = 1_073_741_824;
        for free in [28.3, 28.0, 27.999, (30.0 * GIB - 1.0) / GIB] {
            assert_eq!(own_target_credit_gb(free, 30, 22 * gib), 0.0);
        }
        assert_eq!(own_target_credit_gb(30.0, 30, 8 * gib), 2.0);
        assert_eq!(own_target_credit_gb(31.0, 30, 22 * gib), 2.0);
        assert_eq!(own_target_credit_gb(31.0, 30, gib), 0.25);
        assert_eq!(own_target_credit_gb(31.0, 30, 0), 0.0);
        assert_eq!(own_target_credit_gb(0.5, 1, 8 * gib), 0.0);
        assert_eq!(own_target_credit_gb(1.0, 1, 8 * gib), 0.5);
        assert_eq!(own_target_credit_gb(1.0, 0, 8 * gib), 0.0);
        assert_eq!(own_target_credit_gb(f64::NAN, 30, 8 * gib), 0.0);
        assert_eq!(own_target_credit_gb(f64::MAX, 30, 8 * gib), 2.0);
        assert_eq!(own_target_credit_gb(f64::INFINITY, 30, 8 * gib), 0.0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn disk_floor_is_a_strict_less_than() {
        assert!(disk_below_floor(29.9, 30));
        let just_below = (30.0 * GIB - 1.0) / GIB;
        assert!(disk_below_floor(just_below, 30));
        assert_eq!(
            PressureHold::DiskPressure {
                free_gb: just_below,
                min_gb: 30,
                reserve_gb: 0
            }
            .detail(),
            "held for disk pressure: 30.0 GiB (32212254719 bytes) free on /, floor 30 GiB (32212254720 bytes); governor_min_free_disk_gb holds the work until space is freed"
        );
        assert!(!disk_below_floor(30.0, 30));
        assert!(!disk_below_floor(500.0, 30));
    }

    /// #163: the build floor reports 29 GiB against a 30 GiB floor as closed
    /// (the cargo-slot wait), and keeps the floor visible when it is open.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn build_disk_floor_reports_the_effective_threshold_and_its_margin() {
        let closed = build_disk_floor(29.0, 30);
        assert_eq!(
            (closed.min_free_gb, closed.free_gb, closed.open),
            (30, Some(29.0), false)
        );
        let open = build_disk_floor(30.0, 30);
        assert_eq!(
            (open.min_free_gb, open.free_gb, open.open),
            (30, Some(30.0), true)
        );
        assert!(
            build_disk_floor(1.0, 0).open,
            "a zero floor disables the gate"
        );
        let unreadable = build_disk_floor(f64::MAX, 30);
        assert_eq!((unreadable.free_gb, unreadable.open), (None, true));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn launch_reserve_keeps_the_job_floor_and_reports_the_margin() {
        let hold = disk_hold(35.0, 30, 8).unwrap();
        assert_eq!(
            hold,
            PressureHold::DiskPressure {
                free_gb: 35.0,
                min_gb: 30,
                reserve_gb: 8
            }
        );
        assert!(
            hold.detail()
                .contains("launch reserve 8 GiB, needed free 38 GiB")
        );
        assert_eq!(disk_hold(38.0, 30, 8), None);
        assert_eq!(disk_hold(35.0, 30, 0), None);
        assert_eq!(
            disk_hold(1.0, 0, 8),
            None,
            "zero governor floor disables disk admission"
        );
        assert_eq!(disk_hold(f64::MAX, 30, 8), None, "unreadable disk admits");
        assert!(build_disk_floor(35.0, 30).open);
        TEST_THERMAL_HOLD.with(|cell| {
            cell.set(Some(Some(ThermalHold {
                temperature_c: 91.0,
                ceiling_c: 85,
            })))
        });
        assert_eq!(launch_hold_now(0, 8).unwrap().reason(), THERMAL);
        TEST_THERMAL_HOLD.with(|cell| cell.set(None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn thermal_outranks_disk_and_reasons_are_stable() {
        let hot = ThermalHold {
            temperature_c: 91.0,
            ceiling_c: 85,
        };
        TEST_THERMAL_HOLD.with(|cell| cell.set(Some(Some(hot))));
        let hold = hold_now(0).expect("hot host holds");
        assert_eq!(hold.reason(), THERMAL);
        assert!(hold.detail().contains("91.0 C"));
        TEST_THERMAL_HOLD.with(|cell| cell.set(Some(None)));
        assert_eq!(hold_now(0), None, "cool host with the disk gate off admits");
        // A floor no host can meet holds for disk pressure.
        let hold = hold_now(u64::MAX / 2);
        if let Some(PressureHold::DiskPressure { min_gb, .. }) = hold {
            assert_eq!(hold.unwrap().reason(), DISK_PRESSURE);
            assert_eq!(min_gb, u64::MAX / 2);
        } else {
            // A host whose free space cannot be read never holds.
            assert!(disk_free_gb() >= f64::MAX / 2.0);
        }
        TEST_THERMAL_HOLD.with(|cell| cell.set(None));
    }
}
