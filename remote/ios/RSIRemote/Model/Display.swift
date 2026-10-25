import Foundation

// MARK: - Local time

/// Parses an RFC3339 wire time (`2026-10-17T09:24:00.123456789Z`, with or
/// without fractional seconds, `Z` or a numeric offset). The daemon writes
/// nanoseconds; `ISO8601DateFormatter` reads at most milliseconds, so the
/// fraction is cut to three digits first. Returns nil for anything else.
func parseWireTime(_ rfc3339: String) -> Date? {
    var text = rfc3339.trimmingCharacters(in: .whitespaces)
    guard !text.isEmpty else { return nil }
    if let dot = text.firstIndex(of: "."),
        let timeStart = text.firstIndex(of: "T"), dot > timeStart
    {
        let digitsStart = text.index(after: dot)
        let digitsEnd = text[digitsStart...].firstIndex { !$0.isNumber } ?? text.endIndex
        let digits = text[digitsStart..<digitsEnd]
        guard !digits.isEmpty else { return nil }
        let millis = String((digits + "000").prefix(3))
        text.replaceSubrange(digitsStart..<digitsEnd, with: millis)
        let parser = ISO8601DateFormatter()
        parser.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return parser.date(from: text)
    }
    let parser = ISO8601DateFormatter()
    parser.formatOptions = [.withInternetDateTime]
    return parser.date(from: text)
}

/// A wire time shown in the phone's own time zone and locale: just the time
/// for today, month and day plus time this year, the full date otherwise.
/// Text that is not an RFC3339 time is returned unchanged.
func localTime(
    _ rfc3339: String, now: Date = Date(), timeZone: TimeZone = .current, locale: Locale = .current
) -> String {
    guard let date = parseWireTime(rfc3339) else { return rfc3339 }
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = timeZone
    calendar.locale = locale
    let template: String
    if calendar.isDate(date, inSameDayAs: now) {
        template = "jmm"
    } else if calendar.component(.year, from: date) == calendar.component(.year, from: now) {
        template = "MMMdjmm"
    } else {
        template = "yMMMdjmm"
    }
    let formatter = DateFormatter()
    formatter.calendar = calendar
    formatter.timeZone = timeZone
    formatter.locale = locale
    formatter.setLocalizedDateFormatFromTemplate(template)
    return formatter.string(from: date)
}

// MARK: - Markdown
/// Inline Markdown (bold, italics, strikethrough, `code`, links) with line
/// breaks kept; text that does not parse is returned as plain text.
func inlineMarkdown(_ text: String) -> AttributedString {
    let options = AttributedString.MarkdownParsingOptions(
        interpretedSyntax: .inlineOnlyPreservingWhitespace,
        failurePolicy: .returnPartiallyParsedIfPossible)
    return (try? AttributedString(markdown: text, options: options)) ?? AttributedString(text)
}
