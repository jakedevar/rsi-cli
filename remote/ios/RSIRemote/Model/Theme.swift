import Foundation

/// One sRGB colour, as the TUI writes it (`rgb(r, g, b)` in theme.rs).
struct RGB: Equatable, Hashable, Sendable {
    let r: UInt8
    let g: UInt8
    let b: UInt8

    init(_ r: UInt8, _ g: UInt8, _ b: UInt8) {
        self.r = r
        self.g = g
        self.b = b
    }

    /// `#rrggbb`, lowercase.
    var hex: String { String(format: "#%02x%02x%02x", r, g, b) }
}

/// A TUI theme palette, field for field `CustomPalette` in
/// crates/rsi/src/ui/theme.rs (the Rust snake_case names, camel-cased).
/// ThemeTests parses the bundled theme.rs and checks every value here.
struct ThemePalette: Equatable, Sendable {
    let rosewater, flamingo, pink, mauve, red, maroon, peach, yellow, green: RGB
    let teal, sky, sapphire, blue, lavender: RGB
    let text, subtext1, subtext0, overlay2, overlay1, overlay0: RGB
    let surface2, surface1, surface0, base, mantle, crust: RGB
    let tierBase, tierPanel, tierRaised, tierSelected: RGB
    let darkPurple, groupPowderBlue, epicPurple, storyYellowOrange, taskGray, rawSessionMagenta: RGB

    /// Every field under its theme.rs (snake_case) name, for the parity test.
    var fields: [String: RGB] {
        [
            "rosewater": rosewater, "flamingo": flamingo, "pink": pink, "mauve": mauve,
            "red": red, "maroon": maroon, "peach": peach, "yellow": yellow, "green": green,
            "teal": teal, "sky": sky, "sapphire": sapphire, "blue": blue, "lavender": lavender,
            "text": text, "subtext1": subtext1, "subtext0": subtext0, "overlay2": overlay2,
            "overlay1": overlay1, "overlay0": overlay0, "surface2": surface2, "surface1": surface1,
            "surface0": surface0, "base": base, "mantle": mantle, "crust": crust,
            "tier_base": tierBase, "tier_panel": tierPanel, "tier_raised": tierRaised,
            "tier_selected": tierSelected, "dark_purple": darkPurple,
            "group_powder_blue": groupPowderBlue, "epic_purple": epicPurple,
            "story_yellow_orange": storyYellowOrange, "task_gray": taskGray,
            "raw_session_magenta": rawSessionMagenta,
        ]
    }
}

/// A palette accent a theme's text-accent mapping names (theme.rs `AccentSlot`).
enum AccentSlot: String, Sendable, CaseIterable {
    case primary = "Primary", rosewater = "Rosewater", flamingo = "Flamingo", pink = "Pink"
    case mauve = "Mauve", peach = "Peach", yellow = "Yellow", green = "Green", teal = "Teal"
    case sky = "Sky", sapphire = "Sapphire", blue = "Blue", lavender = "Lavender"
}

/// Which accent colours each kind of metadata (theme.rs `TextAccents`).
struct TextAccents: Equatable, Sendable {
    let model, time, effort, count, cost, path, label: AccentSlot

    var fields: [String: AccentSlot] {
        ["model": model, "time": time, "effort": effort, "count": count,
         "cost": cost, "path": path, "label": label]
    }
}

/// One TUI theme ported to the phone: a `THEME_DEFINITIONS` entry in theme.rs.
struct RemoteTheme: Equatable, Identifiable, Sendable {
    /// The TUI's stable theme key (also what the phone stores).
    let key: String
    let displayName: String
    let palette: ThemePalette
    /// The theme's primary (accent) colour.
    let primary: RGB
    /// The loader spectrum: hue-ordered gradient stops of the TUI's rainbow
    /// activity indicators (`rainbow_palette` samples it).
    let loader: [RGB]
    let textAccents: TextAccents
    /// The light derivation of a ported theme (`lightVariant`); the TUI has none.
    var isLight = false

    var id: String { key }

    /// The colour of an accent slot in this theme (theme.rs `AccentSlot::resolve`).
    func resolve(_ slot: AccentSlot) -> RGB {
        switch slot {
        case .primary: primary
        case .rosewater: palette.rosewater
        case .flamingo: palette.flamingo
        case .pink: palette.pink
        case .mauve: palette.mauve
        case .peach: palette.peach
        case .yellow: palette.yellow
        case .green: palette.green
        case .teal: palette.teal
        case .sky: palette.sky
        case .sapphire: palette.sapphire
        case .blue: palette.blue
        case .lavender: palette.lavender
        }
    }

    // Semantic roles, as theme.rs `semantic_baseline` and the status helpers
    // map them for an opaque theme.
    var canvas: RGB { palette.base }
    var panel: RGB { palette.tierPanel }
    var elevated: RGB { palette.tierRaised }
    var selectedRow: RGB { palette.tierSelected }
    var border: RGB { palette.darkPurple }
    var chrome: RGB { palette.mantle }
    var primaryText: RGB { palette.text }
    var mutedText: RGB { palette.subtext1 }
    var dimText: RGB { palette.overlay1 }
    var running: RGB { palette.green }
    var waiting: RGB { palette.pink }
    var success: RGB { palette.blue }
    var warning: RGB { palette.peach }
    var error: RGB { palette.red }
    var disabled: RGB { palette.overlay0 }
    /// Text on a bright accent badge (theme.rs `active_tab_fg`).
    var onAccent: RGB { palette.crust }
    var assistantRole: RGB { palette.blue }
    var userRole: RGB { palette.green }
    var toolName: RGB { palette.peach }
    var systemEvent: RGB { palette.overlay0 }
    var thinking: RGB { palette.mauve }
    /// Timestamps (the theme's `time` text accent).
    var time: RGB { resolve(textAccents.time) }
    /// Model and provider names (the theme's `model` text accent).
    var model: RGB { resolve(textAccents.model) }
    /// Section labels (the theme's `label` text accent).
    var label: RGB { resolve(textAccents.label) }

    // Markdown, as theme.rs `md_*` and `code_block_bg`.
    var mdHeader: RGB { primary }
    var mdHeaderMinor: RGB { palette.subtext1 }
    var mdBlockquote: RGB { palette.overlay1 }
    var mdListBullet: RGB { palette.overlay1 }
    var mdLink: RGB { palette.blue }
    var mdRule: RGB { palette.surface1 }
    var codeFg: RGB { palette.subtext1 }
    var codeBg: RGB { palette.surface0 }

    /// A session status's colour (theme.rs `status_color`).
    func status(_ status: String) -> RGB {
        switch status {
        case "Starting": palette.yellow
        case "Running": running
        case "WaitingApproval": waiting
        case "Completed": success
        case "Failed": error
        case "Interrupted", "Archived": palette.overlay0
        default: palette.subtext0
        }
    }
}

extension RemoteTheme {
    /// The TUI's default theme (theme.rs `DEFAULT_THEME_INDEX`: Goth).
    static let goth = RemoteTheme(
        key: "goth", displayName: "Goth",
        palette: ThemePalette(
            rosewater: RGB(243, 197, 198),
            flamingo: RGB(225, 134, 134),
            pink: RGB(223, 106, 170),
            mauve: RGB(181, 138, 215),
            red: RGB(229, 83, 105),
            maroon: RGB(170, 76, 98),
            peach: RGB(223, 143, 118),
            yellow: RGB(210, 180, 90),
            green: RGB(111, 191, 141),
            teal: RGB(79, 166, 163),
            sky: RGB(123, 182, 217),
            sapphire: RGB(92, 150, 192),
            blue: RGB(92, 140, 234),
            lavender: RGB(203, 183, 255),
            text: RGB(244, 240, 255),
            subtext1: RGB(203, 196, 221),
            subtext0: RGB(158, 153, 183),
            overlay2: RGB(138, 132, 162),
            overlay1: RGB(118, 112, 144),
            overlay0: RGB(96, 91, 118),
            surface2: RGB(43, 48, 68),
            surface1: RGB(32, 36, 52),
            surface0: RGB(26, 29, 39),
            base: RGB(11, 13, 17),
            mantle: RGB(8, 9, 13),
            crust: RGB(17, 19, 26),
            tierBase: RGB(11, 13, 17),
            tierPanel: RGB(26, 29, 39),
            tierRaised: RGB(32, 36, 52),
            tierSelected: RGB(32, 36, 52),
            darkPurple: RGB(100, 55, 165),
            groupPowderBlue: RGB(165, 201, 224),
            epicPurple: RGB(181, 126, 220),
            storyYellowOrange: RGB(230, 166, 92),
            taskGray: RGB(138, 138, 138),
            rawSessionMagenta: RGB(208, 117, 168)
        ),
        primary: RGB(181, 138, 215),
        loader: [RGB(203, 68, 83), RGB(217, 99, 156), RGB(199, 127, 210), RGB(180, 154, 237), RGB(101, 134, 231), RGB(60, 126, 190), RGB(101, 134, 231), RGB(180, 154, 237), RGB(199, 127, 210), RGB(217, 99, 156)],
        textAccents: TextAccents(
            model: .lavender, time: .sky, effort: .pink, count: .blue,
            cost: .green, path: .rosewater, label: .primary))

    static let rainbow = RemoteTheme(
        key: "raaaainnnnnnbooozzzzzzzz", displayName: "RAAAAINNNNNNBOOOZZZZZZZZ",
        palette: ThemePalette(
            rosewater: RGB(255, 214, 231),
            flamingo: RGB(255, 138, 128),
            pink: RGB(255, 79, 216),
            mauve: RGB(199, 125, 255),
            red: RGB(255, 77, 90),
            maroon: RGB(229, 73, 125),
            peach: RGB(255, 159, 67),
            yellow: RGB(255, 225, 77),
            green: RGB(92, 255, 136),
            teal: RGB(46, 242, 208),
            sky: RGB(86, 217, 255),
            sapphire: RGB(47, 167, 255),
            blue: RGB(95, 124, 255),
            lavender: RGB(184, 167, 255),
            text: RGB(255, 248, 255),
            subtext1: RGB(232, 223, 244),
            subtext0: RGB(200, 189, 217),
            overlay2: RGB(160, 148, 178),
            overlay1: RGB(143, 126, 160),
            overlay0: RGB(119, 103, 137),
            surface2: RGB(48, 34, 77),
            surface1: RGB(33, 24, 55),
            surface0: RGB(21, 16, 36),
            base: RGB(8, 6, 15),
            mantle: RGB(5, 4, 10),
            crust: RGB(5, 4, 10),
            tierBase: RGB(8, 6, 15),
            tierPanel: RGB(21, 16, 36),
            tierRaised: RGB(33, 24, 55),
            tierSelected: RGB(38, 20, 63),
            darkPurple: RGB(167, 107, 255),
            groupPowderBlue: RGB(142, 219, 255),
            epicPurple: RGB(209, 124, 255),
            storyYellowOrange: RGB(255, 177, 74),
            taskGray: RGB(167, 160, 181),
            rawSessionMagenta: RGB(255, 95, 200)
        ),
        primary: RGB(46, 242, 208),
        loader: [RGB(255, 77, 90), RGB(255, 159, 67), RGB(255, 225, 77), RGB(92, 255, 136), RGB(46, 242, 208), RGB(86, 217, 255), RGB(47, 167, 255), RGB(95, 124, 255), RGB(199, 125, 255), RGB(255, 79, 216)],
        textAccents: TextAccents(
            model: .mauve, time: .sky, effort: .peach, count: .yellow,
            cost: .green, path: .pink, label: .primary))

    static let junkyard = RemoteTheme(
        key: "junkyard", displayName: "Junk Yard",
        palette: ThemePalette(
            rosewater: RGB(249, 224, 227),
            flamingo: RGB(237, 115, 133),
            pink: RGB(233, 116, 209),
            mauve: RGB(179, 136, 235),
            red: RGB(240, 87, 93),
            maroon: RGB(173, 78, 103),
            peach: RGB(255, 177, 122),
            yellow: RGB(243, 212, 95),
            green: RGB(102, 188, 126),
            teal: RGB(30, 185, 128),
            sky: RGB(126, 232, 250),
            sapphire: RGB(0, 154, 196),
            blue: RGB(93, 133, 255),
            lavender: RGB(201, 197, 255),
            text: RGB(249, 247, 239),
            subtext1: RGB(225, 222, 210),
            subtext0: RGB(176, 171, 161),
            overlay2: RGB(155, 163, 157),
            overlay1: RGB(139, 145, 140),
            overlay0: RGB(111, 114, 111),
            surface2: RGB(44, 53, 59),
            surface1: RGB(36, 44, 49),
            surface0: RGB(30, 37, 41),
            base: RGB(19, 20, 23),
            mantle: RGB(25, 29, 32),
            crust: RGB(16, 18, 20),
            tierBase: RGB(19, 20, 23),
            tierPanel: RGB(30, 37, 41),
            tierRaised: RGB(36, 44, 49),
            tierSelected: RGB(36, 44, 49),
            darkPurple: RGB(105, 50, 175),
            groupPowderBlue: RGB(138, 172, 192),
            epicPurple: RGB(168, 121, 202),
            storyYellowOrange: RGB(196, 145, 74),
            taskGray: RGB(140, 140, 140),
            rawSessionMagenta: RGB(197, 113, 157)
        ),
        primary: RGB(30, 185, 128),
        loader: [RGB(229, 86, 74), RGB(250, 164, 83), RGB(238, 208, 76), RGB(94, 212, 118), RGB(13, 195, 158), RGB(82, 202, 224), RGB(71, 116, 246), RGB(173, 123, 238), RGB(225, 97, 188)],
        textAccents: TextAccents(
            model: .sky, time: .yellow, effort: .peach, count: .lavender,
            cost: .green, path: .sapphire, label: .primary))

    static let daemon = RemoteTheme(
        key: "d-is-for-devil", displayName: "D. is for Daemon",
        palette: ThemePalette(
            rosewater: RGB(245, 231, 233),
            flamingo: RGB(255, 103, 116),
            pink: RGB(255, 53, 100),
            mauve: RGB(177, 105, 230),
            red: RGB(255, 30, 48),
            maroon: RGB(205, 12, 37),
            peach: RGB(244, 104, 91),
            yellow: RGB(232, 176, 95),
            green: RGB(103, 199, 133),
            teal: RGB(78, 198, 187),
            sky: RGB(104, 192, 235),
            sapphire: RGB(72, 165, 225),
            blue: RGB(113, 138, 235),
            lavender: RGB(203, 186, 255),
            text: RGB(242, 237, 238),
            subtext1: RGB(213, 204, 206),
            subtext0: RGB(179, 168, 171),
            overlay2: RGB(157, 146, 148),
            overlay1: RGB(126, 117, 119),
            overlay0: RGB(91, 84, 86),
            surface2: RGB(51, 48, 49),
            surface1: RGB(31, 29, 30),
            surface0: RGB(17, 16, 16),
            base: RGB(0, 0, 0),
            mantle: RGB(5, 5, 5),
            crust: RGB(0, 0, 0),
            tierBase: RGB(0, 0, 0),
            tierPanel: RGB(16, 15, 15),
            tierRaised: RGB(29, 27, 28),
            tierSelected: RGB(59, 9, 17),
            darkPurple: RGB(156, 92, 220),
            groupPowderBlue: RGB(174, 200, 230),
            epicPurple: RGB(182, 134, 250),
            storyYellowOrange: RGB(230, 164, 110),
            taskGray: RGB(192, 192, 192),
            rawSessionMagenta: RGB(255, 95, 154)
        ),
        primary: RGB(255, 30, 48),
        loader: [RGB(244, 195, 82), RGB(254, 134, 15), RGB(247, 81, 37), RGB(238, 11, 42), RGB(191, 18, 78), RGB(145, 32, 97), RGB(191, 18, 78), RGB(238, 11, 42), RGB(247, 81, 37), RGB(254, 134, 15)],
        textAccents: TextAccents(
            model: .mauve, time: .yellow, effort: .peach, count: .flamingo,
            cost: .green, path: .rosewater, label: .primary))

    /// The themes the Settings picker offers, default first.
    static let all: [RemoteTheme] = [.goth, .rainbow, .junkyard, .daemon]
    static let `default` = RemoteTheme.goth

    /// The theme stored under `key`; an unknown or missing key is the default.
    static func named(_ key: String?) -> RemoteTheme {
        all.first { $0.key == key } ?? .default
    }
}

/// Where the phone keeps the theme choice (UserDefaults; `@AppStorage` reads
/// the same key).
enum ThemeSetting {
    static let key = "rsi.remote.theme"

    static func load(_ defaults: UserDefaults = .standard) -> RemoteTheme {
        RemoteTheme.named(defaults.string(forKey: key))
    }

    static func save(_ theme: RemoteTheme, _ defaults: UserDefaults = .standard) {
        defaults.set(theme.key, forKey: key)
    }
}
