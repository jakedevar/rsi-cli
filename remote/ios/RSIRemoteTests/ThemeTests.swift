import XCTest
@testable import RSIRemote

/// The ported themes against the TUI's own source: theme.rs is bundled as a
/// test resource (project.yml), so every value here is checked against the
/// file the TUI compiles, not against a copy.
final class ThemeTests: XCTestCase {
    private static func themeSource() throws -> String {
        let bundle = Bundle(for: ThemeTests.self)
        let url = try XCTUnwrap(bundle.url(forResource: "theme", withExtension: "rs"))
        return try String(contentsOf: url, encoding: .utf8)
    }

    /// The text from `opening` to the next `closing`.
    private static func block(_ source: String, _ opening: String, _ closing: String) throws -> String {
        let start = try XCTUnwrap(source.range(of: opening), "missing \(opening)")
        let end = try XCTUnwrap(
            source.range(of: closing, range: start.upperBound..<source.endIndex), "unclosed \(opening)")
        return String(source[start.upperBound..<end.lowerBound])
    }

    private static func rgb(_ match: Regex<(Substring, Substring, Substring, Substring)>.Match) -> RGB {
        RGB(UInt8(match.output.1)!, UInt8(match.output.2)!, UInt8(match.output.3)!)
    }

    private static func palette(_ source: String, _ name: String) throws -> [String: RGB] {
        let body = try block(source, "const \(name): CustomPalette = CustomPalette {", "};")
        var fields: [String: RGB] = [:]
        for match in body.matches(of: /(\w+): rgb\((\d+), (\d+), (\d+)\)/) {
            fields[String(match.output.1)] = RGB(
                UInt8(match.output.2)!, UInt8(match.output.3)!, UInt8(match.output.4)!)
        }
        return fields
    }

    private static func loader(_ source: String, _ name: String) throws -> [RGB] {
        let body = try block(source, "const \(name): [Color;", "];")
        return body.matches(of: /rgb\((\d+), (\d+), (\d+)\)/).map(rgb)
    }

    private static func accents(_ source: String, _ name: String) throws -> [String: String] {
        let body = try block(source, "const \(name): TextAccents = TextAccents {", "};")
        var fields: [String: String] = [:]
        for match in body.matches(of: /(\w+): AccentSlot::(\w+)/) {
            fields[String(match.output.1)] = String(match.output.2)
        }
        return fields
    }

    /// One `THEME_DEFINITIONS` entry, found by its key.
    private struct Definition {
        let displayName: String
        let palette: String
        let primary: RGB
        let loader: String
        let accents: String
    }

    private static func definitions(_ source: String) throws -> [String: Definition] {
        let table = try block(source, "const THEME_DEFINITIONS: [ThemeDefinition;", "\n];")
        var out: [String: Definition] = [:]
        let entry = /ThemeDefinition::new\(\s*"([^"]+)",\s*"([^"]+)",\s*&(\w+),\s*\d+,\s*rgb\((\d+), (\d+), (\d+)\),[\s\S]*?\.with_loader\(&(\w+)\)\s*\.with_text_accents\(&(\w+)\)/
        for match in table.matches(of: entry) {
            let o = match.output
            out[String(o.1)] = Definition(
                displayName: String(o.2), palette: String(o.3),
                primary: RGB(UInt8(o.4)!, UInt8(o.5)!, UInt8(o.6)!),
                loader: String(o.7), accents: String(o.8))
        }
        return out
    }

    func testThePickerOffersTheTuiDefaultAndTheThreeNamedThemes() {
        XCTAssertEqual(
            RemoteTheme.all.map(\.key), ["goth", "raaaainnnnnnbooozzzzzzzz", "junkyard", "d-is-for-devil"])
        XCTAssertEqual(RemoteTheme.default.key, "goth")
    }

    func testTheDefaultIsTheTuiDefaultTheme() throws {
        let source = try Self.themeSource()
        let index = try XCTUnwrap(
            source.firstMatch(of: /const DEFAULT_THEME_INDEX: usize = (\d+);/).flatMap { Int($0.output.1) })
        let table = try Self.block(source, "const THEME_DEFINITIONS: [ThemeDefinition;", "\n];")
        let keys = table.matches(of: /ThemeDefinition::new\(\s*"([^"]+)"/).map { String($0.output.1) }
        XCTAssertEqual(keys[index], RemoteTheme.default.key)
    }

    func testEveryPortedThemeEqualsThemeRs() throws {
        let source = try Self.themeSource()
        let definitions = try Self.definitions(source)
        for theme in RemoteTheme.all {
            let definition = try XCTUnwrap(definitions[theme.key], "theme.rs has no \(theme.key)")
            XCTAssertEqual(theme.displayName, definition.displayName, theme.key)
            XCTAssertEqual(theme.primary, definition.primary, "\(theme.key) primary")

            let palette = try Self.palette(source, definition.palette)
            XCTAssertEqual(palette.count, 36, "\(definition.palette) field count")
            XCTAssertEqual(Set(theme.palette.fields.keys), Set(palette.keys), theme.key)
            for (field, value) in palette {
                XCTAssertEqual(
                    theme.palette.fields[field]?.hex, value.hex, "\(theme.key).\(field)")
            }

            let loader = try Self.loader(source, definition.loader)
            XCTAssertFalse(loader.isEmpty, definition.loader)
            XCTAssertEqual(theme.loader.map(\.hex), loader.map(\.hex), "\(theme.key) loader")

            let accents = try Self.accents(source, definition.accents)
            XCTAssertEqual(accents.count, 7, definition.accents)
            XCTAssertEqual(theme.textAccents.fields.mapValues(\.rawValue), accents, "\(theme.key) accents")
        }
    }

    func testRolesFollowTheTuiMapping() {
        let goth = RemoteTheme.goth
        XCTAssertEqual(goth.canvas.hex, "#0b0d11")
        XCTAssertEqual(goth.running, goth.palette.green)
        XCTAssertEqual(goth.waiting, goth.palette.pink)
        XCTAssertEqual(goth.status("WaitingApproval"), goth.palette.pink)
        XCTAssertEqual(goth.status("Failed"), goth.palette.red)
        XCTAssertEqual(goth.status("Completed"), goth.palette.blue)
        XCTAssertEqual(goth.status("Starting"), goth.palette.yellow)
        XCTAssertEqual(goth.status("SomethingNew"), goth.palette.subtext0)
        XCTAssertEqual(goth.selectedRow, goth.palette.tierSelected)
        XCTAssertEqual(goth.label, goth.primary)
        XCTAssertEqual(RemoteTheme.daemon.time, RemoteTheme.daemon.palette.yellow)
        XCTAssertEqual(RemoteTheme.rainbow.model, RemoteTheme.rainbow.palette.mauve)
        XCTAssertEqual(RemoteTheme.junkyard.mdHeader, RGB(30, 185, 128))
    }

    private func scratchDefaults() throws -> UserDefaults {
        let name = "rsi.remote.theme-tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        addTeardownBlock { UserDefaults(suiteName: name)?.removePersistentDomain(forName: name) }
        return defaults
    }

    func testThemeChoiceRoundTripsThroughUserDefaults() throws {
        let defaults = try scratchDefaults()
        XCTAssertEqual(ThemeSetting.load(defaults).key, "goth")
        for theme in RemoteTheme.all {
            ThemeSetting.save(theme, defaults)
            XCTAssertEqual(defaults.string(forKey: ThemeSetting.key), theme.key)
            XCTAssertEqual(ThemeSetting.load(defaults), theme)
        }
    }

    func testAnUnknownStoredKeyFallsBackToTheDefault() throws {
        let defaults = try scratchDefaults()
        defaults.set("gruvbox-warm", forKey: ThemeSetting.key)
        XCTAssertEqual(ThemeSetting.load(defaults).key, "goth")
        defaults.set("", forKey: ThemeSetting.key)
        XCTAssertEqual(ThemeSetting.load(defaults).key, "goth")
        XCTAssertEqual(RemoteTheme.named(nil).key, "goth")
        XCTAssertEqual(RemoteTheme.named("JUNKYARD").key, "goth")
        XCTAssertEqual(RemoteTheme.named("junkyard").displayName, "Junk Yard")
    }
}
