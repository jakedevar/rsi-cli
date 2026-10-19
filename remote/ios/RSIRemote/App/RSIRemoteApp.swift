import SwiftUI

@main
struct RSIRemoteApp: App {
    @State private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
        }
    }
}

struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        TabView {
            Tab("Projects", systemImage: "folder") {
                ProjectsView()
            }
            Tab("Waiting", systemImage: "hourglass") {
                WaitingView()
            }
            .badge(model.waitingCount)
            Tab("Settings", systemImage: "gearshape") {
                SettingsView()
            }
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
