import SwiftUI

/// The fixture conversation, offline, in the real conversation list. Opened by the
/// launch argument `-markdownDemo <n>` so a phone that cannot be tapped from the Mac
/// can still be screenshotted (#503): `0` (or any other text) is the whole
/// conversation, `1`...`7` the question and that one answer. It touches no saved
/// host and makes no request.
struct MarkdownDemoView: View {
    static let launchKey = "markdownDemo"

    static func isRequested(_ defaults: UserDefaults = .standard) -> Bool {
        defaults.string(forKey: launchKey) != nil
    }

    /// The events to show for a launch value: all, or the question and answer `n`.
    static func events(for value: String?) -> [ChatEvent] {
        let all = MarkdownFixture.events
        guard let value, let part = Int(value.trimmingCharacters(in: .whitespaces)),
            (1...MarkdownFixture.parts.count).contains(part)
        else { return all }
        return [all[0], all[part]]
    }

    var body: some View {
        NavigationStack {
            ConversationScroll(
                turns: ChatTranscript.build(Self.events(for: UserDefaults.standard.string(forKey: Self.launchKey))),
                assistantName: "Assistant", header: { EmptyView() }, footer: { EmptyView() }
            )
            .environment(\.markdownDemoOpensZoom, UserDefaults.standard.string(forKey: "markdownDemoZoom") != nil)
            .navigationTitle("Markdown")
            .navigationBarTitleDisplayMode(.inline)
        }
    }
}
