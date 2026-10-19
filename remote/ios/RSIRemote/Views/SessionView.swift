import SwiftUI

/// One session's latest history page, its open decisions and their answer forms.
struct SessionView: View {
    @Environment(AppModel.self) private var model
    let project: WireProject
    let summary: SessionSummary

    @State private var events: [HistoryEvent] = []
    @State private var decisions: [Decision] = []
    @State private var error: RemoteError?
    @State private var refreshToken = 0

    var body: some View {
        List {
            if !decisions.isEmpty {
                Section("Waiting on you") {
                    ForEach(decisions) { DecisionRowView(decision: $0) }
                }
            }
            Section("Answer") {
                AnswerSectionView(
                    project: project, sessionId: summary.id, decisions: decisions,
                    refreshToken: refreshToken)
            }
            Section("Latest activity") {
                if events.isEmpty, error == nil {
                    Text("No activity yet").foregroundStyle(.secondary)
                }
                ForEach(events.reversed()) { EventRowView(event: $0) }
            }
            if let error {
                Section { Text(error.message).foregroundStyle(.red) }
            }
        }
        .navigationTitle(summary.ownTitle.isEmpty ? "Session" : summary.ownTitle)
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await load() }
        .task { await load() }
    }

    private func load() async {
        guard let client = model.client else {
            error = .notConfigured
            return
        }
        do {
            async let history = client.history(project: project.id, session: summary.id)
            async let open = client.decisions(project: project.id, session: summary.id)
            events = try await history.items
            decisions = (try? await open.items.filter(\.isOpen)) ?? []
            error = nil
            refreshToken += 1
        } catch let failure as RemoteError {
            error = failure
        } catch {
            self.error = .invalidResponse("history")
        }
    }
}

struct EventRowView: View {
    let event: HistoryEvent

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Label(label, systemImage: icon)
                .font(.caption.bold())
                .foregroundStyle(.secondary)
            Text(text + (event.truncated ? " …" : ""))
                .font(event.speaker == .tool ? .caption.monospaced() : .body)
                .lineLimit(event.speaker == .tool ? 4 : 12)
        }
    }

    private var text: String {
        if event.speaker == .tool, let tool = event.toolName { "\(tool): \(event.text)" } else { event.text }
    }

    private var label: String {
        switch event.speaker {
        case .user: "You"
        case .assistant: "Agent"
        case .tool: "Tool"
        case .thinking: "Thinking"
        case .system: "System"
        }
    }

    private var icon: String {
        switch event.speaker {
        case .user: "person"
        case .assistant: "sparkles"
        case .tool: "wrench.and.screwdriver"
        case .thinking: "brain"
        case .system: "info.circle"
        }
    }
}

struct DecisionRowView: View {
    let decision: Decision

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(decision.title).font(.subheadline.bold())
            if !decision.detail.isEmpty {
                Text(decision.detail).font(.caption)
            }
            ForEach(Array(decision.questions.enumerated()), id: \.offset) { _, question in
                VStack(alignment: .leading, spacing: 2) {
                    if let header = question.header, !header.isEmpty {
                        Text(header).font(.caption.bold()).foregroundStyle(.secondary)
                    }
                    Text(question.question).font(.caption)
                    ForEach(question.options, id: \.label) { option in
                        Text("• \(option.label)").font(.caption2).foregroundStyle(.secondary)
                    }
                }
            }
        }
    }
}
