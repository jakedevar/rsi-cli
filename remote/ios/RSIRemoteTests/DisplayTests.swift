import Foundation
import XCTest

@testable import RSIRemote

/// Local-time display and Markdown block parsing (no network, no UI).
final class DisplayTests: XCTestCase {
    private let pacific = TimeZone(identifier: "America/Los_Angeles")!
    private let tokyo = TimeZone(identifier: "Asia/Tokyo")!
    private let us = Locale(identifier: "en_US")

    func testWireTimesParseWithNanosecondsMillisecondsNoneAndOffsets() throws {
        let expected = try XCTUnwrap(parseWireTime("2026-10-17T09:24:00Z"))
        XCTAssertEqual(parseWireTime("2026-10-17T09:24:00.000000000Z"), expected)
        XCTAssertEqual(parseWireTime("2026-10-17T02:24:00-07:00"), expected)
        let nanos = try XCTUnwrap(parseWireTime("2026-10-17T09:24:00.123456789Z"))
        XCTAssertEqual(nanos.timeIntervalSince(expected), 0.123, accuracy: 0.0005)
        XCTAssertNil(parseWireTime("soon"))
        XCTAssertNil(parseWireTime(""))
        XCTAssertNil(parseWireTime("2026-10-17T09:24:00.Z"))
    }

    func testTodayShowsOnlyTheLocalClockTime() {
        let now = parseWireTime("2026-10-17T20:00:00Z")!
        XCTAssertEqual(
            localTime("2026-10-17T16:24:00.000000000Z", now: now, timeZone: pacific, locale: us),
            "9:24\u{202F}AM")
        XCTAssertEqual(
            localTime("2026-10-17T09:24:00.5Z", now: parseWireTime("2026-10-17T10:00:00Z")!,
                timeZone: tokyo, locale: us),
            "6:24\u{202F}PM")
    }

    func testOlderTimesAddTheLocalDate() {
        let now = parseWireTime("2026-10-19T12:00:00Z")!
        // 03:30 UTC on the 18th is still the 17th in California.
        XCTAssertEqual(
            localTime("2026-10-18T03:30:00Z", now: now, timeZone: pacific, locale: us),
            "Oct 17 at 8:30\u{202F}PM")
        // 23:00 UTC on 2025-12-31 is already 2026 in Tokyo: same year, no year shown.
        XCTAssertEqual(
            localTime("2025-12-31T23:00:00Z", now: now, timeZone: tokyo, locale: us),
            "Jan 1 at 8:00\u{202F}AM")
        XCTAssertEqual(
            localTime("2025-06-01T00:00:00Z", now: now, timeZone: tokyo, locale: us),
            "Jun 1, 2025 at 9:00\u{202F}AM")
    }

    func testNonTimeTextIsShownUnchanged() {
        XCTAssertEqual(localTime("soon"), "soon")
        XCTAssertEqual(localTime(""), "")
    }

    func testInlineMarkdownStylesAndKeepsBreaks() {
        let rendered = inlineMarkdown("a **b** `c` [d](https://example.com)\nnext")
        XCTAssertEqual(String(rendered.characters), "a b c d\nnext")
        let bold = rendered.runs.first { String(rendered[$0.range].characters) == "b" }
        XCTAssertEqual(bold?.inlinePresentationIntent, .stronglyEmphasized)
        let link = rendered.runs.first { String(rendered[$0.range].characters) == "d" }
        XCTAssertEqual(link?.link, URL(string: "https://example.com"))
        XCTAssertEqual(String(inlineMarkdown("**unclosed").characters), "**unclosed")
    }
}
