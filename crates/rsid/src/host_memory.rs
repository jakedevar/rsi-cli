//! Host memory source and daemon-side build admission for hosts that have no
//! cargo-slot governor client (#2937).
//!
//! On Linux every cargo unit runs behind `scripts/cargo-slot`, which asks the
//! resource governor (`governor.rs`) for a slot and compares `MemAvailable`
//! with `governor_min_avail_mem_gb`. A Mac runs the unit directly (the slot
//! wrapper is `/usr/bin/env`: cargo-slot uses Linux-only tools), so nothing
//! bounded the work: a 16 GB Mac Mini satellite could start several cargo
//! units of the 120 MB `rsid` binary at once and swap. The daemon therefore
//! holds a new cargo unit itself (`agent_jobs`, `held_for_capacity`, queued,
//! no unit) while either
//!
//! * as many cargo units already run as the live `governor_build_slots`
//!   (default 2 on a 16 GB Mac, [`GovernorPolicy::MACOS_16GB_DEFAULT`]), or
//! * available memory is below the live `governor_min_avail_mem_gb` (default
//!   4 GiB there).
//!
//! The two settings reach the gate through a process-global the store
//! publishes on every write ([`GovernorPolicy::live_job_gate`]), the way
//! `thermal_guard::set_ceiling_c` does (#2951).
//!
//! "Available" on macOS is `vm_stat`'s free + inactive + speculative pages
//! times the page size. Purgeable pages are left out on purpose: they are
//! already counted among the active and inactive pages, so adding them would
//! overstate what a build can claim. An unreadable source never holds (the
//! governor's rule too); the slot cap still bounds concurrency.
//!
//! Linux is unchanged: [`job_capacity_hold`] is only consulted where
//! [`CAPACITY_GATE_ENABLED`] is set.

use crate::governor::GovernorPolicy;

const GIB: f64 = 1_073_741_824.0;

/// Whether the daemon itself bounds cargo-unit concurrency and memory. Off on
/// Linux (cargo-slot and the governor do it) and on platforms with no job
/// runtime; on for macOS.
pub const CAPACITY_GATE_ENABLED: bool = cfg!(target_os = "macos");

/// Pages `vm_stat` reports as claimable without paging out an application.
/// Returns the byte count: (free + inactive + speculative) x page size.
#[cfg(any(test, target_os = "macos"))]
#[must_use]
pub(crate) fn parse_vm_stat_available_bytes(output: &str) -> Option<u64> {
    let mut lines = output.lines();
    let header = lines.next()?;
    let page_size: u64 = header
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    if page_size == 0 {
        return None;
    }
    let (mut free, mut inactive, mut speculative) = (None, None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let pages = || value.trim().trim_end_matches('.').parse::<u64>().ok();
        match name.trim() {
            "Pages free" => free = pages(),
            "Pages inactive" => inactive = pages(),
            "Pages speculative" => speculative = pages(),
            _ => {}
        }
    }
    free.zip(inactive)
        .zip(speculative)
        .and_then(|((free, inactive), speculative)| {
            free.checked_add(inactive)?
                .checked_add(speculative)?
                .checked_mul(page_size)
        })
}

/// Available memory in bytes: `vm_stat` on macOS, `None` where there is no
/// source (Linux reads `MemAvailable` in `governor.rs`, unchanged).
#[cfg(target_os = "macos")]
#[must_use]
pub fn read_available_bytes() -> Option<u64> {
    let output = std::process::Command::new("/usr/bin/vm_stat")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_vm_stat_available_bytes(&String::from_utf8_lossy(&output.stdout))
}

/// No memory source on this platform: the gates pass.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn read_available_bytes() -> Option<u64> {
    None
}

/// Why a new cargo unit is held on a host with no cargo-slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobCapacityHold {
    /// As many cargo units run as the policy's `build_slots`.
    Slots { running: u32, cap: u32 },
    /// Available memory is below the policy's `min_avail_mem_gb`.
    Memory { available_gb: f64, min_gb: u64 },
}

impl JobCapacityHold {
    /// One line for the job log.
    #[must_use]
    pub fn detail(self) -> String {
        match self {
            Self::Slots { running, cap } => format!(
                "held for capacity: {running} of {cap} cargo build slots are in use; the job starts when one ends"
            ),
            Self::Memory {
                available_gb,
                min_gb,
            } => format!(
                "held for capacity: {available_gb:.1} GiB of memory is available, below the {min_gb} GiB governor_min_avail_mem_gb floor; the job starts when memory frees"
            ),
        }
    }
}

/// The pure admission rule: the slot cap first, then the memory floor
/// (`min_avail_mem_gb` 0 disables it; `None` available memory passes).
#[must_use]
pub fn job_capacity_hold(
    policy: &GovernorPolicy,
    running_units: u32,
    available_gb: Option<f64>,
) -> Option<JobCapacityHold> {
    if running_units >= policy.build_slots {
        return Some(JobCapacityHold::Slots {
            running: running_units,
            cap: policy.build_slots,
        });
    }
    let available_gb = available_gb?;
    (policy.min_avail_mem_gb > 0 && available_gb < policy.min_avail_mem_gb as f64).then_some(
        JobCapacityHold::Memory {
            available_gb,
            min_gb: policy.min_avail_mem_gb,
        },
    )
}

/// The hold in force now for a host with `running_units` cargo units already
/// running, against this platform's default policy with the operator's live
/// `governor_build_slots` and `governor_min_avail_mem_gb` applied
/// ([`GovernorPolicy::live_job_gate`], #2951). `None` on hosts where the
/// daemon does not gate ([`CAPACITY_GATE_ENABLED`]).
#[must_use]
pub fn host_capacity_hold(running_units: u32) -> Option<JobCapacityHold> {
    #[cfg(test)]
    if let Some(forced) = TEST_CAPACITY_HOLD.with(std::cell::Cell::get) {
        return forced;
    }
    if !CAPACITY_GATE_ENABLED {
        return None;
    }
    job_capacity_hold(
        &GovernorPolicy::live_job_gate(),
        running_units,
        read_available_bytes().map(|bytes| bytes as f64 / GIB),
    )
}

// Test seam: the hold this thread sees (`Some(None)` = open), `None` = real.
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_CAPACITY_HOLD: std::cell::Cell<Option<Option<JobCapacityHold>>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM_STAT: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                               10000.\n\
Pages active:                            200000.\n\
Pages inactive:                           50000.\n\
Pages speculative:                         5000.\n\
Pages throttled:                              0.\n\
Pages wired down:                         90000.\n\
Pages purgeable:                          30000.\n";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn vm_stat_available_is_free_inactive_and_speculative_pages() {
        // (10000 + 50000 + 5000) pages x 16384 bytes; purgeable is not added.
        assert_eq!(
            parse_vm_stat_available_bytes(VM_STAT),
            Some(65_000 * 16_384)
        );
        // An Intel Mac reports 4096-byte pages.
        let intel = VM_STAT.replace("16384", "4096");
        assert_eq!(parse_vm_stat_available_bytes(&intel), Some(65_000 * 4_096));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn vm_stat_garbage_is_unreadable_not_zero() {
        for bad in [
            "",
            "garbage",
            "Mach Virtual Memory Statistics: (page size of 0 bytes)\nPages free: 1.\nPages inactive: 1.\nPages speculative: 1.\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 1.\nPages inactive: 1.\n",
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: x.\nPages inactive: 1.\nPages speculative: 1.\n",
        ] {
            assert_eq!(parse_vm_stat_available_bytes(bad), None, "{bad:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_16gb_mac_policy_bounds_slots_and_memory() {
        let mac = GovernorPolicy::MACOS_16GB_DEFAULT;
        assert_eq!(
            (mac.build_slots, mac.lander_slots, mac.min_avail_mem_gb),
            (2, 1, 4)
        );
        // Open: one unit running, plenty of memory.
        assert_eq!(job_capacity_hold(&mac, 1, Some(8.0)), None);
        // The second unit is the last; the third waits for a slot.
        assert_eq!(
            job_capacity_hold(&mac, 2, Some(8.0)),
            Some(JobCapacityHold::Slots { running: 2, cap: 2 })
        );
        // Low memory holds even the first unit; the boundary is strict.
        assert_eq!(
            job_capacity_hold(&mac, 0, Some(3.9)),
            Some(JobCapacityHold::Memory {
                available_gb: 3.9,
                min_gb: 4
            })
        );
        assert_eq!(job_capacity_hold(&mac, 0, Some(4.0)), None);
        // An unreadable source never holds; the slot cap still applies.
        assert_eq!(job_capacity_hold(&mac, 1, None), None);
        assert!(job_capacity_hold(&mac, 2, None).is_some());
        // The slot cap reports first when both gates close.
        assert!(matches!(
            job_capacity_hold(&mac, 2, Some(0.5)),
            Some(JobCapacityHold::Slots { .. })
        ));
        // A zero memory floor disables that gate.
        let off = GovernorPolicy {
            min_avail_mem_gb: 0,
            ..mac
        };
        assert_eq!(job_capacity_hold(&off, 0, Some(0.1)), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn linux_policy_and_gate_are_unchanged() {
        let linux = GovernorPolicy::LINUX_DEFAULT;
        assert_eq!(
            (
                linux.build_slots,
                linux.lander_slots,
                linux.check_slots,
                linux.max_load,
                linux.min_free_disk_gb,
                linux.min_avail_mem_gb,
                linux.max_workers_slice_gb
            ),
            (4, 5, 2, 0, 30, 16, 30)
        );
        #[cfg(target_os = "linux")]
        {
            assert_eq!(GovernorPolicy::platform_default(), linux);
            assert!(!CAPACITY_GATE_ENABLED);
            // Without the seam the daemon never gates a Linux job itself.
            assert_eq!(host_capacity_hold(99), None);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn capacity_holds_name_their_gate() {
        let slots = JobCapacityHold::Slots { running: 2, cap: 2 }.detail();
        assert!(slots.contains("2 of 2 cargo build slots"), "{slots}");
        let memory = JobCapacityHold::Memory {
            available_gb: 3.5,
            min_gb: 4,
        }
        .detail();
        assert!(
            memory.contains("3.5 GiB") && memory.contains("4 GiB"),
            "{memory}"
        );
    }
}
