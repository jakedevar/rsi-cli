import Foundation

/// When a screen's data was last read and how its latest refresh went (#350).
/// A failed refresh keeps the last good data on screen with its age and names
/// the failure; the screen reads again by itself on a backoff while shown.
struct Freshness: Sendable, Equatable {
    private(set) var updatedAt: Date?
    private(set) var error: RemoteError?
    private(set) var failedAt: Date?
    private(set) var failures = 0
    private(set) var retryAfter: Int?

    /// A refresh read everything.
    mutating func succeeded(at date: Date) {
        updatedAt = date
        error = nil
        failedAt = nil
        failures = 0
        retryAfter = nil
    }

    /// A refresh failed. A cancelled one (the screen went away) changes nothing.
    mutating func failed(_ error: RemoteError, at date: Date, retryAfter: Int? = nil) {
        guard error != .cancelled else { return }
        self.error = error
        failedAt = date
        failures += 1
        self.retryAfter = retryAfter
    }

    /// The backoff after the latest failure: 5 s doubling to 2 min, or a
    /// longer `Retry-After` (at most 10 min).
    var backoff: Duration {
        var seconds = 5
        for _ in 1..<max(1, min(failures, 8)) { seconds = min(seconds * 2, 120) }
        if let retryAfter { seconds = max(seconds, min(retryAfter, 600)) }
        return .seconds(seconds)
    }

    /// When the screen should read again by itself: `every` after a good
    /// read, the backoff after a failed one, now if it never read.
    func nextRefresh(every: Duration) -> Date? {
        if let failedAt, error != nil { return failedAt.addingTimeInterval(seconds(backoff)) }
        return updatedAt?.addingTimeInterval(seconds(every))
    }

    /// How long to wait before that, at least one second.
    func wait(every: Duration, now: Date) -> Duration {
        guard let next = nextRefresh(every: every) else { return .seconds(1) }
        return .milliseconds(Int(max(1, next.timeIntervalSince(now)) * 1000))
    }

    /// No good read, a failed latest refresh, or a good read older than `maxAge` seconds.
    func isStale(_ maxAge: TimeInterval, now: Date) -> Bool {
        guard let updatedAt, error == nil else { return true }
        return now.timeIntervalSince(updatedAt) > maxAge
    }

    /// The line shown over data the screen already has; nil while it is
    /// fresh and fine. `every` is the screen's own refresh period.
    func status(now: Date, every: Duration) -> String? {
        if let error {
            var parts = [error.headline]
            if let updatedAt { parts.append("showing data from \(ageText(now.timeIntervalSince(updatedAt)))") }
            parts.append(retryText(now: now, every: every))
            return parts.joined(separator: " · ")
        }
        guard let updatedAt, now.timeIntervalSince(updatedAt) >= 90 else { return nil }
        return "Updated \(ageText(now.timeIntervalSince(updatedAt)))"
    }

    /// "retrying in 12 s", counting down to "retrying now".
    func retryText(now: Date, every: Duration) -> String {
        guard let next = nextRefresh(every: every) else { return "retrying now" }
        let left = Int(next.timeIntervalSince(now).rounded(.up))
        return left > 0 ? "retrying in \(left) s" : "retrying now"
    }
}

/// How old something is, in the words of a status line.
func ageText(_ seconds: TimeInterval) -> String {
    let whole = max(0, Int(seconds))
    switch whole {
    case ..<10: return "just now"
    case ..<60: return "\(whole) s ago"
    case ..<3600: return "\(whole / 60) min ago"
    case ..<86_400: return "\(whole / 3600) h ago"
    default: return "\(whole / 86_400) d ago"
    }
}

private func seconds(_ duration: Duration) -> TimeInterval {
    let (whole, fraction) = duration.components
    return TimeInterval(whole) + TimeInterval(fraction) / 1e18
}
