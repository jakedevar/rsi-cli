import SwiftUI

@main
struct RSIRemoteApp: App {
    @State private var model = AppModel(outbox: Outbox(store: FileOutboxStore.standard()))

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
        }
    }
}

struct RootView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    @AppStorage(ThemeSetting.key) private var themeKey = RemoteTheme.default.key
    @AppStorage(Appearance.key) private var appearanceKey = Appearance.default.rawValue

    @State private var selection = RootTab.initial()

    var body: some View {
        if MarkdownDemoView.isRequested() {
            // `-markdownDemo 1`: the fixture conversation, offline (#503).
            MarkdownDemoView()
                .themedRoot(RemoteTheme.named(themeKey), appearance: Appearance.named(appearanceKey))
        } else if ProviderUsageDemoView.isRequested() {
            // `-providerUsageFixture full`: the provider usage screen, offline (#504).
            ProviderUsageDemoView()
                .themedRoot(RemoteTheme.named(themeKey), appearance: Appearance.named(appearanceKey))
        } else {
            tabs
        }
    }

    private var tabs: some View {
        TabView(selection: $selection) {
            Tab("Dashboard", systemImage: "gauge.with.dots.needle.33percent", value: RootTab.dashboard) {
                DashboardView()
            }
            Tab("Projects", systemImage: "folder", value: RootTab.projects) {
                ProjectsView()
            }
            Tab("Waiting", systemImage: "hourglass", value: RootTab.waiting) {
                WaitingView()
            }
            .badge(model.waitingCount)
            Tab("Settings", systemImage: "gearshape", value: RootTab.settings) {
                SettingsView()
            }
        }
        .themedRoot(RemoteTheme.named(themeKey), appearance: Appearance.named(appearanceKey))
        // Messages saved before the app last closed go out now; launch-argument
        // checks for the device run here too.
        .task { await model.start() }
        // Back in the foreground: the screen on show reads again if its data is stale.
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { model.foregrounded() }
        }
    }
}

/// App and contract versions shown in Settings.
enum AppInfo {
    static var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(short) (\(build))"
    }

    /// The gateway contract versions this build speaks.
    static let contract = "RSI Remote " + supportedContracts.joined(separator: " + ")
}

/// The root tabs. A launch argument `-initialTab projects|waiting|settings`
/// (UserDefaults' argument domain, for example `devicectl device process
/// launch … -initialTab waiting`) opens one directly, so a phone that cannot
/// be tapped from the Mac can still be screenshotted on each screen (#492).
enum RootTab: String, Hashable {
    case dashboard, projects, waiting, settings

    static let launchKey = "initialTab"

    static func initial(defaults: UserDefaults = .standard) -> RootTab {
        parse(defaults.string(forKey: launchKey))
    }

    static func parse(_ raw: String?) -> RootTab {
        guard let raw else { return .dashboard }
        return RootTab(rawValue: raw.trimmingCharacters(in: .whitespaces).lowercased())
            ?? .dashboard
    }
}
