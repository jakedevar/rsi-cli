//! Provider usage-limit exhaustion (#572): the provider's own retry-after, a
//! bounded backoff floor, and the typed hold refusal that keeps automated
//! dispatch (scheduled wakes, mail delivery, continuations) from re-attempting
//! an exhausted account in a loop.
//!
//! Everything here is pure. The hold itself is derived from rows the daemon
//! already writes (see `store::provider_exhaustion`), so it survives a restart
//! and needs no schema change.

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};

use crate::error::DaemonError;

/// Stable code carried by the typed refusal of an automated dispatch while the
/// provider account is exhausted.
pub const PROVIDER_USAGE_LIMIT_HOLD_CODE: &str = "provider_usage_limit_hold";

const HOLD_UNTIL_MARKER: &str = "until=";
/// Phrase the Codex CLI puts in front of its machine-readable retry time.
const RETRY_AFTER_MARKER: &str = "try again at ";
/// Only the head of the provider text is scanned for the marker.
const MAX_SCANNED_CHARS: usize = 2_000;
/// A retry-after further out than this is treated as unparseable garbage.
const MAX_RETRY_AFTER: Duration = Duration::days(40);
/// First backoff after a usage-limit failure when no retry-after was given.
const BACKOFF_BASE: Duration = Duration::minutes(5);
/// Hard ceiling on the backoff floor.
const BACKOFF_CAP: Duration = Duration::hours(6);
/// Doubling steps before the cap applies.
const BACKOFF_MAX_DOUBLINGS: u32 = 7;

/// Parse the provider-reported retry-after from a usage-limit message such as
/// `... or try again at Sep 7th, 2026 1:05 PM.` The provider prints no zone, so
/// the time is read as UTC. Unknown formats yield `None`; this never panics.
pub fn parse_codex_retry_after(message: &str) -> Option<DateTime<Utc>> {
    let head: String = message.chars().take(MAX_SCANNED_CHARS).collect();
    let lower = head.to_ascii_lowercase();
    let start = lower.rfind(RETRY_AFTER_MARKER)? + RETRY_AFTER_MARKER.len();
    // `to_ascii_lowercase` preserves byte offsets, so `start` indexes `head`.
    let rest = head.get(start..)?;
    let mut tokens = rest.split_whitespace();
    let month = month_number(tokens.next()?)?;
    let day = day_number(tokens.next()?)?;
    let year: i32 = tokens
        .next()?
        .trim_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
        .filter(|year| (2000..=2200).contains(year))?;
    let (hour, minute) = clock(tokens.next()?)?;
    let meridiem = tokens
        .next()?
        .trim_matches(|c: char| !c.is_ascii_alphabetic())
        .to_ascii_lowercase();
    let hour24 = match (hour, meridiem.as_str()) {
        (1..=11, "am") => hour,
        (12, "am") => 0,
        (12, "pm") => 12,
        (1..=11, "pm") => hour + 12,
        _ => return None,
    };
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let time = NaiveTime::from_hms_opt(hour24, minute, 0)?;
    Some(date.and_time(time).and_utc())
}

fn month_number(token: &str) -> Option<u32> {
    let letters: String = token
        .trim_matches(|c: char| !c.is_ascii_alphabetic())
        .to_ascii_lowercase();
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    if letters.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|abbrev| letters.starts_with(abbrev))
        .map(|index| index as u32 + 1)
}

fn day_number(token: &str) -> Option<u32> {
    let digits: String = token
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let suffix = token
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .get(digits.len()..)?
        .to_ascii_lowercase();
    if !matches!(suffix.as_str(), "" | "st" | "nd" | "rd" | "th") {
        return None;
    }
    digits.parse().ok().filter(|day| (1..=31).contains(day))
}

fn clock(token: &str) -> Option<(u32, u32)> {
    let (hour, minute) = token
        .trim_matches(|c: char| !c.is_ascii_digit() && c != ':')
        .split_once(':')?;
    Some((
        hour.parse().ok().filter(|hour| (1..=12).contains(hour))?,
        minute.parse().ok().filter(|minute| *minute < 60)?,
    ))
}

/// The backoff floor after `attempts` consecutive usage-limit failures
/// (`attempts` of 0 or 1 is the first failure): 5 min doubling to a 6 h cap.
pub fn exhaustion_backoff(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(BACKOFF_MAX_DOUBLINGS);
    (BACKOFF_BASE * (1 << doublings)).min(BACKOFF_CAP)
}

/// When the next attempt may run after a usage-limit failure at `failed_at`:
/// no earlier than the provider's retry-after, and never sooner than the
/// bounded backoff floor, so a stale or zone-shifted retry-after cannot turn
/// into a tight retry loop.
pub fn hold_until(error_text: &str, failed_at: DateTime<Utc>, attempts: u32) -> DateTime<Utc> {
    let floor = failed_at + exhaustion_backoff(attempts);
    match parse_codex_retry_after(error_text) {
        Some(retry_after) if retry_after <= failed_at + MAX_RETRY_AFTER => retry_after.max(floor),
        _ => floor,
    }
}

/// An active hold on automated Codex dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageLimitHold {
    pub until: DateTime<Utc>,
    /// The exact provider text of the failure that caused the hold.
    pub provider_text: String,
    pub session_id: uuid::Uuid,
}

/// The typed refusal for an automated dispatch held until `until`.
pub fn hold_error(until: DateTime<Utc>) -> DaemonError {
    DaemonError::PolicyDenied(format!(
        "{PROVIDER_USAGE_LIMIT_HOLD_CODE}: {HOLD_UNTIL_MARKER}{}",
        until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ))
}

/// The hold deadline carried by a [`hold_error`], if `error` is one.
pub fn hold_error_until(error: &DaemonError) -> Option<DateTime<Utc>> {
    let text = error.to_string();
    let after = text.split_once(PROVIDER_USAGE_LIMIT_HOLD_CODE)?.1;
    let value = after.split_once(HOLD_UNTIL_MARKER)?.1;
    let value = value.split_whitespace().next()?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const OBSERVED: &str = "Error running remote compact task: You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 7th, 2026 1:05 PM.";

    #[test]
    fn retry_after_parses_the_observed_provider_format() {
        assert_eq!(
            parse_codex_retry_after(OBSERVED),
            Some(Utc.with_ymd_and_hms(2026, 9, 7, 13, 5, 0).unwrap())
        );
        assert_eq!(
            parse_codex_retry_after("try again at Aug 15th, 2026 1:29 AM."),
            Some(Utc.with_ymd_and_hms(2026, 8, 15, 1, 29, 0).unwrap())
        );
        assert_eq!(
            parse_codex_retry_after("try again at December 2, 2026 12:00 AM"),
            Some(Utc.with_ymd_and_hms(2026, 12, 2, 0, 0, 0).unwrap())
        );
        assert_eq!(
            parse_codex_retry_after("or try again at Jan 1st, 2027 12:30 PM."),
            Some(Utc.with_ymd_and_hms(2027, 1, 1, 12, 30, 0).unwrap())
        );
    }

    #[test]
    fn retry_after_is_absent_for_messages_without_a_time() {
        for message in [
            "",
            "You've hit your usage limit.",
            "You've hit your usage limit. Retry later.",
            "try again at",
            "try again at ",
        ] {
            assert_eq!(parse_codex_retry_after(message), None, "{message:?}");
        }
    }

    #[test]
    fn retry_after_garbage_never_panics_and_yields_no_time() {
        for message in [
            "try again at tomorrow",
            "try again at Sep 31st, 2026 1:05 PM.",
            "try again at Sep 7th, 2026 13:05 PM.",
            "try again at Sep 7th, 2026 1:75 PM.",
            "try again at Sep 7th, 2026 1:05",
            "try again at Smarch 7th, 2026 1:05 PM.",
            "try again at Sep 0th, 2026 1:05 PM.",
            "try again at Sep 7xx, 2026 1:05 PM.",
            "try again at Sep 7th, 99999999999999999999 1:05 PM.",
            "try again at \u{1F600}\u{1F600} \u{1F600} \u{1F600} \u{1F600}",
            "TRY AGAIN AT Sep \u{e9}th, 2026 1:05 PM.",
        ] {
            assert_eq!(parse_codex_retry_after(message), None, "{message:?}");
        }
        let long = format!("{}try again at Sep 7th, 2026 1:05 PM.", "é".repeat(5_000));
        assert_eq!(parse_codex_retry_after(&long), None);
    }

    #[test]
    fn hold_is_no_earlier_than_retry_after_and_never_below_the_backoff_floor() {
        let failed = Utc.with_ymd_and_hms(2026, 9, 4, 1, 36, 31).unwrap();
        // Provider time wins when it is later than the floor.
        assert_eq!(
            hold_until(OBSERVED, failed, 1),
            Utc.with_ymd_and_hms(2026, 9, 7, 13, 5, 0).unwrap()
        );
        // No time: the bounded backoff, growing per attempt and capped.
        assert_eq!(
            hold_until("Retry later.", failed, 1),
            failed + Duration::minutes(5)
        );
        assert_eq!(
            hold_until("Retry later.", failed, 3),
            failed + Duration::minutes(20)
        );
        assert_eq!(
            hold_until("Retry later.", failed, 50),
            failed + Duration::hours(6)
        );
        // A retry-after already in the past cannot shorten the floor.
        let late = Utc.with_ymd_and_hms(2026, 9, 7, 14, 0, 0).unwrap();
        assert_eq!(hold_until(OBSERVED, late, 1), late + Duration::minutes(5));
        // An absurd retry-after is ignored in favour of the floor.
        assert_eq!(
            hold_until("try again at Sep 7th, 2099 1:05 PM.", failed, 1),
            failed + Duration::minutes(5)
        );
    }

    #[test]
    fn hold_error_round_trips_its_deadline() {
        let until = Utc.with_ymd_and_hms(2026, 9, 7, 13, 5, 0).unwrap();
        let error = hold_error(until);
        assert!(error.to_string().contains(PROVIDER_USAGE_LIMIT_HOLD_CODE));
        assert_eq!(hold_error_until(&error), Some(until));
        assert_eq!(
            hold_error_until(&DaemonError::PolicyDenied("something else".into())),
            None
        );
    }
}
