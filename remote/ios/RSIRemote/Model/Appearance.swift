import Foundation

/// Dark, light, or whatever the phone is set to. The ported TUI themes are all
/// dark, so the default stays dark (#502).
enum Appearance: String, Sendable, CaseIterable, Identifiable {
    case dark, light, system

    static let key = "rsi.remote.appearance"
    static let `default` = Appearance.dark

    var id: String { rawValue }

    var label: String {
        switch self {
        case .dark: "Dark"
        case .light: "Light"
        case .system: "Match phone"
        }
    }

    /// Whether to show the light variant; `systemIsDark` is the phone's setting.
    func isLight(systemIsDark: Bool) -> Bool {
        switch self {
        case .dark: false
        case .light: true
        case .system: !systemIsDark
        }
    }

    static func named(_ raw: String?) -> Appearance {
        raw.flatMap(Appearance.init(rawValue:)) ?? .default
    }
}

extension RemoteTheme {
    /// This theme for the given appearance: itself, or its light variant.
    func resolved(light: Bool) -> RemoteTheme { light ? lightVariant : self }

    /// A light version of a ported theme: Catppuccin Latte neutrals and accents
    /// (the family the TUI palettes are named after) with the theme's own primary
    /// darkened to read on white. Not a TUI port; the TUI has no light theme.
    var lightVariant: RemoteTheme {
        if isLight { return self }
        let latte = ThemePalette(
            rosewater: RGB(220, 138, 120), flamingo: RGB(221, 120, 120), pink: RGB(234, 118, 203),
            mauve: RGB(136, 57, 239), red: RGB(210, 15, 57), maroon: RGB(230, 69, 83),
            peach: RGB(254, 100, 11), yellow: RGB(223, 142, 29), green: RGB(64, 160, 43),
            teal: RGB(23, 146, 153), sky: RGB(4, 165, 229), sapphire: RGB(32, 159, 181),
            blue: RGB(30, 102, 245), lavender: RGB(114, 135, 253),
            text: RGB(76, 79, 105), subtext1: RGB(92, 95, 119), subtext0: RGB(108, 111, 133),
            overlay2: RGB(124, 127, 147), overlay1: RGB(140, 143, 161), overlay0: RGB(156, 160, 176),
            surface2: RGB(172, 176, 190), surface1: RGB(188, 192, 204), surface0: RGB(204, 208, 218),
            base: RGB(239, 241, 245), mantle: RGB(230, 233, 239), crust: RGB(220, 224, 232),
            tierBase: RGB(239, 241, 245), tierPanel: RGB(255, 255, 255),
            tierRaised: RGB(255, 255, 255), tierSelected: RGB(220, 224, 232),
            darkPurple: RGB(114, 135, 253), groupPowderBlue: palette.groupPowderBlue,
            epicPurple: palette.epicPurple, storyYellowOrange: palette.storyYellowOrange,
            taskGray: palette.taskGray, rawSessionMagenta: palette.rawSessionMagenta)
        return RemoteTheme(
            key: key, displayName: displayName, palette: latte,
            primary: primary.darkened(by: 0.4), loader: loader, textAccents: textAccents,
            isLight: true)
    }
}

extension RGB {
    /// Mixed toward black by `amount` (0 unchanged, 1 black).
    func darkened(by amount: Double) -> RGB {
        func channel(_ value: UInt8) -> UInt8 { UInt8((Double(value) * (1 - amount)).rounded()) }
        return RGB(channel(r), channel(g), channel(b))
    }

    /// WCAG relative luminance, 0 (black) to 1 (white).
    var luminance: Double {
        func linear(_ value: UInt8) -> Double {
            let c = Double(value) / 255
            return c <= 0.03928 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    /// WCAG contrast ratio against `other`, 1 to 21.
    func contrast(with other: RGB) -> Double {
        let (a, b) = (luminance, other.luminance)
        return (max(a, b) + 0.05) / (min(a, b) + 0.05)
    }
}
