import SwiftUI

extension RGB {
    var color: Color {
        Color(.sRGB, red: Double(r) / 255, green: Double(g) / 255, blue: Double(b) / 255, opacity: 1)
    }
}

extension EnvironmentValues {
    /// `-markdownDemoZoom 1`: a loaded inline image opens its zoom view by itself, so the
    /// zoom screen can be screenshotted where nothing can be tapped (#503).
    @Entry var markdownDemoOpensZoom = false
    /// The active TUI theme (Settings → Theme).
    @Entry var theme: RemoteTheme = .default
}

extension RemoteTheme {
    /// A tone's colour in this theme (answers, deliveries).
    func color(_ tone: AnswerTone) -> Color {
        switch tone {
        case .idle, .busy: mutedText.color
        case .ok: running.color
        case .warn: warning.color
        case .error: error.color
        }
    }

    /// The loader spectrum as a closed-loop gradient.
    var spectrum: Gradient { Gradient(colors: (loader + loader.prefix(1)).map(\.color)) }
}

/// Root styling: the theme's canvas, text and accent, the chosen appearance (the
/// ported themes are dark; light is their derived variant) and the chrome bars
/// in the theme's mantle.
struct ThemedRoot: ViewModifier {
    let base: RemoteTheme
    var appearance: Appearance = .default
    @Environment(\.colorScheme) private var systemScheme

    private var theme: RemoteTheme {
        base.resolved(light: appearance.isLight(systemIsDark: systemScheme == .dark))
    }

    private var scheme: ColorScheme? {
        switch appearance {
        case .dark: .dark
        case .light: .light
        case .system: nil
        }
    }

    func body(content: Content) -> some View {
        content
            .environment(\.theme, theme)
            .tint(theme.primary.color)
            .foregroundStyle(theme.primaryText.color)
            .preferredColorScheme(scheme)
            .background(theme.canvas.color.ignoresSafeArea())
    }
}

/// A List or Form on the theme's canvas with theme-coloured bars; its rows
/// take `themedRow()` (a row background does not pass down from the List).
struct ThemedList: ViewModifier {
    @Environment(\.theme) private var theme

    func body(content: Content) -> some View {
        content
            .scrollContentBackground(.hidden)
            .background(theme.canvas.color.ignoresSafeArea())
            .toolbarBackground(theme.chrome.color, for: .navigationBar, .tabBar)
            .toolbarBackground(.visible, for: .navigationBar, .tabBar)
    }
}

/// A list row on the theme's panel tier.
struct ThemedRow: ViewModifier {
    @Environment(\.theme) private var theme

    func body(content: Content) -> some View {
        content.listRowBackground(theme.panel.color)
    }
}

extension View {
    func themedRoot(_ theme: RemoteTheme, appearance: Appearance = .default) -> some View {
        modifier(ThemedRoot(base: theme, appearance: appearance))
    }
    func themedList() -> some View { modifier(ThemedList()) }
    func themedRow() -> some View { modifier(ThemedRow()) }
}

/// A thin strip of the theme's loader spectrum, the TUI's rainbow accent.
struct SpectrumStrip: View {
    let theme: RemoteTheme
    var height: CGFloat = 3

    var body: some View {
        LinearGradient(gradient: theme.spectrum, startPoint: .leading, endPoint: .trailing)
            .frame(height: height)
            .clipShape(Capsule())
            .accessibilityHidden(true)
    }
}
