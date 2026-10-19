//! Operator setting `qa_target_max_gib` (#2509): the size above which the
//! shared full-library QA Cargo target is wiped before the next run.
//!
//! The hourly QA reuses one target. Cargo never deletes a superseded
//! artifact, so after every run trims what the run replaced
//! (`rsid::qa_target_trim`) this cap is the backstop: a target that is still
//! over it (a bug, a toolchain change, a first run after the setting was
//! introduced) is emptied before it can fill the disk. The store mirrors the
//! live setting here so the job launcher reads it without the runtime config.

use std::sync::atomic::{AtomicU32, Ordering};

/// Default cap in GiB. One warm SHA's footprint is about 15-20 GiB.
pub const QA_TARGET_MAX_GIB_DEFAULT: u32 = 30;
/// Smallest accepted non-zero value (below it a single cold build exceeds it).
pub const QA_TARGET_MAX_GIB_MIN: u32 = 10;
/// Largest accepted value.
pub const QA_TARGET_MAX_GIB_MAX: u32 = 500;

static MAX_GIB: AtomicU32 = AtomicU32::new(QA_TARGET_MAX_GIB_DEFAULT);

/// The live cap in GiB; 0 means the target is never wiped for its size.
#[must_use]
pub fn current_gib() -> u32 {
    MAX_GIB.load(Ordering::Relaxed)
}

/// Publish the operator's value (the store calls this on every accepted write).
pub fn set_gib(gib: u32) {
    MAX_GIB.store(gib, Ordering::Relaxed);
}

/// The live cap in bytes, `None` when the cap is off.
#[must_use]
pub fn current_bytes() -> Option<u64> {
    let gib = current_gib();
    (gib > 0).then(|| u64::from(gib) * 1024 * 1024 * 1024)
}

/// Whether `gib` is an accepted setting: 0 (off) or inside the hard range.
#[must_use]
pub const fn is_valid_gib(gib: u64) -> bool {
    gib == 0 || (gib >= QA_TARGET_MAX_GIB_MIN as u64 && gib <= QA_TARGET_MAX_GIB_MAX as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_off_and_the_range_is_checked() {
        assert!(is_valid_gib(0));
        assert!(is_valid_gib(10));
        assert!(is_valid_gib(500));
        assert!(!is_valid_gib(9));
        assert!(!is_valid_gib(501));
        assert_eq!(QA_TARGET_MAX_GIB_DEFAULT, 30);
    }
}
