use chrono::{DateTime, Duration, Months, Utc};

use crate::types::{Recurrence, ScheduleSpec};

/// Compute the next fire time after `after`, given the schedule spec.
/// For Recurrence::Once, returns None (job should be disabled after firing).
/// For recurring jobs, advances from `after` by the recurrence interval
/// until the result is strictly in the future relative to `now`.
pub fn next_fire_time(
    spec: &ScheduleSpec,
    after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match spec.recurrence {
        Recurrence::Once => None,
        Recurrence::EverySeconds(n) => Some(advance_fixed(after, Duration::seconds(n as i64), now)),
        Recurrence::EveryMinutes(n) => Some(advance_fixed(after, Duration::minutes(n as i64), now)),
        Recurrence::EveryHours(n) => Some(advance_fixed(after, Duration::hours(n as i64), now)),
        Recurrence::EveryDays(n) => Some(advance_fixed(after, Duration::days(n as i64), now)),
        Recurrence::EveryWeeks(n) => Some(advance_fixed(after, Duration::weeks(n as i64), now)),
        Recurrence::EveryMonths(n) => Some(advance_calendar_months(after, n, now)),
        Recurrence::EveryYears(n) => Some(advance_calendar_months(after, n * 12, now)),
    }
}

/// Advance `from` by `step` until the result is > `now`.
fn advance_fixed(from: DateTime<Utc>, step: Duration, now: DateTime<Utc>) -> DateTime<Utc> {
    if step.num_seconds() <= 0 {
        return from + Duration::seconds(1); // safety: prevent infinite loop
    }
    let mut t = from + step;
    // Fast-forward: compute how many steps needed to pass `now`
    if t <= now {
        let elapsed = (now - from).num_seconds();
        let step_secs = step.num_seconds();
        let steps_needed = elapsed / step_secs; // integer division, rounds down
        t = from + Duration::seconds(step_secs * steps_needed);
        if t <= now {
            t += step;
        }
    }
    t
}

/// Advance `from` by `months` calendar months until the result is > `now`.
fn advance_calendar_months(from: DateTime<Utc>, months: u32, now: DateTime<Utc>) -> DateTime<Utc> {
    if months == 0 {
        return from + Duration::days(30); // safety
    }
    let mut t = from;
    loop {
        t = t
            .checked_add_months(Months::new(months))
            .unwrap_or_else(|| t + Duration::days(months as i64 * 30));
        if t > now {
            return t;
        }
    }
}

/// Validate a schedule spec is legal for creation or enabling.
/// Rejects `Once` schedules whose anchor is already in the past
/// (they would fire on the very next scheduler poll, bypassing the user's intent).
/// Recurring jobs are always accepted — the daemon catches up from the past anchor.
pub fn validate_creatable(spec: &ScheduleSpec, now: DateTime<Utc>) -> Result<(), String> {
    if matches!(spec.recurrence, Recurrence::Once) && spec.anchor <= now {
        return Err(format!(
            "one-shot job anchor {} is in the past (now {}); \
             once-only jobs must have a future fire time",
            spec.anchor.to_rfc3339(),
            now.to_rfc3339()
        ));
    }
    Ok(())
}

/// Compute the initial `next_fire_at` from the anchor.
/// If the anchor is in the future, that's the first fire time.
/// If the anchor is in the past, it depends on recurrence:
/// - Once: fire immediately (anchor itself)
/// - Recurring: advance to the next future fire time
pub fn initial_next_fire_at(spec: &ScheduleSpec, now: DateTime<Utc>) -> DateTime<Utc> {
    if spec.anchor > now {
        return spec.anchor;
    }
    match spec.recurrence {
        Recurrence::Once => spec.anchor, // fire immediately on next check
        _ => next_fire_time(spec, spec.anchor, now).unwrap_or(spec.anchor),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
    }

    #[test]
    fn test_once_returns_none() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        assert_eq!(
            next_fire_time(&spec, utc(2026, 1, 1, 0, 0, 0), utc(2026, 1, 1, 0, 0, 1)),
            None
        );
    }

    #[test]
    fn test_every_hour_catches_up() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryHours(1),
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        // Daemon was down for 3 days — should advance to next future time, not fire 72 times
        let now = utc(2026, 1, 4, 0, 0, 0);
        let last_fired = utc(2026, 1, 1, 0, 0, 0);
        let next = next_fire_time(&spec, last_fired, now).unwrap();
        assert!(next > now);
        assert!(next <= now + Duration::hours(1));
    }

    #[test]
    fn test_every_minute_basic() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryMinutes(5),
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        let now = utc(2026, 1, 1, 0, 3, 0);
        let last_fired = utc(2026, 1, 1, 0, 0, 0);
        let next = next_fire_time(&spec, last_fired, now).unwrap();
        assert_eq!(next, utc(2026, 1, 1, 0, 5, 0));
    }

    #[test]
    fn test_every_day_catches_up() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryDays(1),
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        let now = utc(2026, 1, 10, 12, 0, 0);
        let last_fired = utc(2026, 1, 1, 0, 0, 0);
        let next = next_fire_time(&spec, last_fired, now).unwrap();
        assert!(next > now);
        assert!(next <= now + Duration::days(1));
    }

    #[test]
    fn test_every_week() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryWeeks(2),
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        let now = utc(2026, 1, 1, 0, 0, 1);
        let last_fired = utc(2026, 1, 1, 0, 0, 0);
        let next = next_fire_time(&spec, last_fired, now).unwrap();
        assert_eq!(next, utc(2026, 1, 15, 0, 0, 0));
    }

    #[test]
    fn test_every_month_advances() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryMonths(1),
            anchor: utc(2026, 1, 15, 0, 0, 0),
        };
        let now = utc(2026, 4, 1, 0, 0, 0);
        let next = next_fire_time(&spec, utc(2026, 1, 15, 0, 0, 0), now).unwrap();
        assert!(next > now);
        assert_eq!(next.month(), 4); // April 15th
    }

    #[test]
    fn test_every_year() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryYears(1),
            anchor: utc(2026, 6, 1, 0, 0, 0),
        };
        let now = utc(2027, 1, 1, 0, 0, 0);
        let last_fired = utc(2026, 6, 1, 0, 0, 0);
        let next = next_fire_time(&spec, last_fired, now).unwrap();
        assert!(next > now);
        assert_eq!(next, utc(2027, 6, 1, 0, 0, 0));
    }

    #[test]
    fn test_initial_future_anchor() {
        let now = utc(2026, 1, 1, 0, 0, 0);
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryDays(1),
            anchor: utc(2026, 6, 1, 0, 0, 0),
        };
        assert_eq!(initial_next_fire_at(&spec, now), utc(2026, 6, 1, 0, 0, 0));
    }

    #[test]
    fn test_initial_past_anchor_once() {
        let now = utc(2026, 6, 1, 0, 0, 0);
        let spec = ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        // Once jobs with past anchor fire immediately
        assert_eq!(initial_next_fire_at(&spec, now), utc(2026, 1, 1, 0, 0, 0));
    }

    #[test]
    fn test_initial_past_anchor_recurring() {
        let now = utc(2026, 3, 1, 0, 0, 0);
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryDays(7),
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        let next = initial_next_fire_at(&spec, now);
        assert!(next > now);
        assert!(next <= now + Duration::days(7));
    }

    #[test]
    fn test_every_seconds() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::EverySeconds(30),
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        let now = utc(2026, 1, 1, 0, 0, 45);
        let last_fired = utc(2026, 1, 1, 0, 0, 0);
        let next = next_fire_time(&spec, last_fired, now).unwrap();
        assert_eq!(next, utc(2026, 1, 1, 0, 1, 0));
    }

    #[test]
    fn test_zero_interval_safety() {
        let spec = ScheduleSpec {
            recurrence: Recurrence::EverySeconds(0),
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        let now = utc(2026, 1, 1, 0, 0, 0);
        // Should not infinite loop — returns safety value
        let next = next_fire_time(&spec, utc(2026, 1, 1, 0, 0, 0), now);
        assert!(next.is_some());
    }

    use chrono::Datelike;

    #[test]
    fn test_validate_creatable_once_past_rejects() {
        let now = utc(2026, 6, 1, 12, 0, 0);
        let spec = ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: utc(2026, 6, 1, 11, 59, 59),
        };
        assert!(validate_creatable(&spec, now).is_err());
    }

    #[test]
    fn test_validate_creatable_once_future_accepts() {
        let now = utc(2026, 6, 1, 12, 0, 0);
        let spec = ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: utc(2026, 6, 1, 12, 0, 1),
        };
        assert!(validate_creatable(&spec, now).is_ok());
    }

    #[test]
    fn test_validate_creatable_recurring_past_accepts() {
        let now = utc(2026, 6, 1, 12, 0, 0);
        let spec = ScheduleSpec {
            recurrence: Recurrence::EveryDays(1),
            anchor: utc(2026, 1, 1, 0, 0, 0),
        };
        assert!(validate_creatable(&spec, now).is_ok());
    }

    #[test]
    fn test_validate_creatable_once_at_now_rejects() {
        let now = utc(2026, 6, 1, 12, 0, 0);
        let spec = ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: utc(2026, 6, 1, 12, 0, 0), // exactly now — also past
        };
        assert!(validate_creatable(&spec, now).is_err());
    }
}
