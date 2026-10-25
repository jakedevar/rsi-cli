//! Operator setting `paste_retention_referenced_days` (#86): how long the
//! daemon keeps a pasted image (`~/.rsi/paste/rsid`) that a message or a
//! conversation once named.
//!
//! Such an image is part of the operator's conversation history, so removing
//! it is opt-in: the default 0 keeps it forever. A value of N days lets the
//! retention pass (`rsid::paste_retention`) remove a referenced image last
//! uploaded more than N days ago, unless a message a provider may still read
//! names it. Abandoned uploads (never named anywhere) follow their own fixed
//! 7-day rule and are not governed by this setting. The store mirrors the live
//! value here so the pass reads it without the runtime config.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Default: 0, referenced images are kept forever.
pub const PASTE_RETENTION_REFERENCED_DAYS_DEFAULT: u32 = 0;
/// Smallest accepted non-zero value: never shorter than the abandoned-upload
/// age (7 days).
pub const PASTE_RETENTION_REFERENCED_DAYS_MIN: u32 = 7;
/// Largest accepted value (10 years).
pub const PASTE_RETENTION_REFERENCED_DAYS_MAX: u32 = 3650;

static DAYS: AtomicU32 = AtomicU32::new(PASTE_RETENTION_REFERENCED_DAYS_DEFAULT);

/// The live value in days; 0 means referenced images are kept forever.
#[must_use]
pub fn current_days() -> u32 {
    DAYS.load(Ordering::Relaxed)
}

/// Publish the operator's value (the store calls this on every accepted write).
pub fn set_days(days: u32) {
    DAYS.store(days, Ordering::Relaxed);
}

/// The live age after which a referenced image may be removed, `None` while
/// the setting is 0 (keep forever).
#[must_use]
pub fn referenced_max_age() -> Option<Duration> {
    max_age_for_days(current_days())
}

/// `days` as an age, `None` for 0.
#[must_use]
pub const fn max_age_for_days(days: u32) -> Option<Duration> {
    if days == 0 {
        None
    } else {
        Some(Duration::from_secs(days as u64 * 24 * 60 * 60))
    }
}

/// Whether `days` is an accepted setting: 0 (keep forever) or inside the range.
#[must_use]
pub const fn is_valid_days(days: u64) -> bool {
    days == 0
        || (days >= PASTE_RETENTION_REFERENCED_DAYS_MIN as u64
            && days <= PASTE_RETENTION_REFERENCED_DAYS_MAX as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_keeps_forever_and_the_range_is_checked() {
        assert_eq!(PASTE_RETENTION_REFERENCED_DAYS_DEFAULT, 0);
        assert!(is_valid_days(0));
        assert!(is_valid_days(7));
        assert!(is_valid_days(3650));
        assert!(!is_valid_days(6));
        assert!(!is_valid_days(3651));
        assert_eq!(max_age_for_days(0), None);
        assert_eq!(
            max_age_for_days(7),
            Some(Duration::from_secs(7 * 24 * 60 * 60))
        );
    }
}
