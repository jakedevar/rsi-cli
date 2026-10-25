import SwiftUI
import XCTest

@testable import RSIRemote

// Renders a fixture session through the real row views, folded and opened, dark
// and light, to PNG files for the evidence note (#502). The assertion is that a
// non-empty image came out; the pictures are read by eye. Files go to
// $RSI_SNAPSHOT_DIR, else /tmp/rsi-502-snapshots; an unwritable folder is not a failure.

@MainActor
final class ConversationSnapshotTests: XCTestCase {
    private func ev(_ id: Int, _ role: ChatRole, _ text: String, tool: String? = nil, phase: ToolPhase? = nil) -> ChatEvent {
        ChatEvent(
            id: String(id), sequence: id, role: role, text: text, createdAt: "2026-10-22T18:0\(id % 10):00Z",
            toolName: tool, phase: phase)
    }

    private var fixture: [ChatEvent] {
        [
            ev(1, .user, "Why is the **landing queue** stuck? Check `rsid` and tell me the next step."),
            ev(2, .thinking, "The queue entry is probably waiting on a gate. Look at the log first."),
            ev(3, .tool, "{\"command\": \"rsi-rpc ListRollingQueue\"}", tool: "Bash", phase: .use),
            ev(4, .tool, "3 entries: 1 gating, 2 queued", tool: "Bash", phase: .result),
            ev(5, .tool, "{\"path\": \"docs/agents/landing-and-gates.md\"}", tool: "Read", phase: .use),
            ev(6, .tool, "# Landing and gates\n(long file…)", tool: "Read", phase: .result),
            ev(7, .assistant, "It is **gating**, not stuck:\n\n- the full QA is running\n- two sources wait behind it\n\n```\nrsi-rpc ListRollingQueue\n```\n\nNothing needs you."),
            ev(8, .user, "Thanks. Merge the docs change when it is green."),
            ev(9, .system, "Seat rotated; the new manager has the handoff."),
            ev(10, .tool, "{\"command\": \"git log -1\"}", tool: "Bash", phase: .use),
        ]
    }

    private func render(_ name: String, theme: RemoteTheme, open: Set<String>) throws {
        let rows = ChatTranscript.rows(ChatTranscript.build(fixture))
        var disclosure = Disclosure()
        for id in open { disclosure.toggle(id) }
        let view = VStack(alignment: .leading, spacing: 0) {
            ForEach(rows) { row in
                RowView(row: row, assistantName: "Agent", disclosure: disclosure, toggle: { _ in })
            }
        }
        .padding(16)
        .frame(width: 390)
        .environment(\.theme, theme)
        .foregroundStyle(theme.primaryText.color)
        .tint(theme.primary.color)
        .background(theme.canvas.color)
        .environment(\.colorScheme, theme.isLight ? .light : .dark)
        let renderer = ImageRenderer(content: view)
        renderer.scale = 2
        let png = try XCTUnwrap(renderer.uiImage?.pngData(), "no image for \(name)")
        XCTAssertGreaterThan(png.count, 5_000)
        let dir = ProcessInfo.processInfo.environment["RSI_SNAPSHOT_DIR"] ?? "/tmp/rsi-502-snapshots"
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent(name + ".png"))
    }

    func testFixtureConversationRendersFoldedAndOpenInDarkAndLight() throws {
        let turns = ChatTranscript.build(fixture)
        let tools = turns[1].parts.compactMap { part -> String? in
            if case .tools = part { part.id } else { nil }
        }
        let thinking = turns[1].parts.first.map(\.id) ?? ""
        let calls = turns[1].calls.map { "call:" + $0.id }
        for theme in [RemoteTheme.goth, RemoteTheme.goth.lightVariant] {
            let tag = theme.isLight ? "light" : "dark"
            try render("conversation-\(tag)-folded", theme: theme, open: [])
            try render("conversation-\(tag)-open", theme: theme, open: Set(tools + calls + [thinking]))
        }
    }
}
