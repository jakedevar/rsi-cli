import SwiftUI

/// Every session that waits on the operator, across the exposed projects.
struct WaitingView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.theme) private var theme

    var body: some View {
        NavigationStack {
            Group {
                if model.host == nil {
                    ContentUnavailableView(
                        "No RSI host", systemImage: "network.slash",
                        description: Text("Set the hub's Tailscale host name in Settings."))
                } else if let error = model.waitingError, model.waiting.isEmpty {
                    ErrorView(error: error, freshness: model.waitingFreshness, every: AppModel.waitingEvery) {
                        Task { await model.refreshWaiting() }
                    }
                } else if model.waiting.isEmpty && model.loadingWaiting {
                    ProgressView("Checking for decisions…")
                } else if model.waiting.isEmpty {
                    ContentUnavailableView(
                        "Nothing waiting", systemImage: "checkmark.circle",
                        description: Text(
                            model.partlyCheckedProjects.isEmpty
                                ? "No manager or session is waiting on you."
                                : "Nothing waiting in the sessions checked. \(partlyCheckedText(model.partlyCheckedProjects))"))
                } else {
                    List {
                        FreshnessRow(freshness: model.waitingFreshness, every: AppModel.waitingEvery)
                        ForEach(model.waiting) { item in
                            Section {
                                if let error = item.error {
                                    Text(error).font(.caption).foregroundStyle(theme.warning.color)
                                }
                                if item.decisions.isEmpty, item.error == nil {
                                    Text("Waiting on you; details are not available yet.")
                                        .font(.caption).foregroundStyle(.secondary)
                                }
                                ForEach(item.decisions) { DecisionRowView(decision: $0) }
                                AnswerSectionView(
                                    project: item.project, sessionId: item.session.id,
                                    decisions: item.decisions, refreshToken: model.waitingGeneration)
                                NavigationLink("Open session") {
                                    SessionView(project: item.project, summary: item.session)
                                }
                            } header: {
                                Text("\(item.project.name) · \(item.session.ownTitle)")
                                    .foregroundStyle(theme.label.color)
                            }
                            .themedRow()
                        }
                        PartlyCheckedNote(projects: model.partlyCheckedProjects)
                    }
                }
            }
            .themedList()
            .navigationTitle("Waiting")
            .refreshable { await model.refreshWaiting() }
            // Reads on appear unless fresh (session lists included), then every
            // minute (on a backoff after a failure) while shown.
            .task(id: model.refreshKey) { await model.keepWaitingFresh() }
        }
    }
}

/// Says which projects had more sessions than the hub's first page, so a
/// waiting session there may not be listed (#350).
struct PartlyCheckedNote: View {
    @Environment(\.theme) private var theme
    let projects: [String]

    var body: some View {
        if !projects.isEmpty {
            Text(partlyCheckedText(projects))
                .font(.caption).foregroundStyle(theme.mutedText.color)
                .themedRow()
        }
    }
}

func partlyCheckedText(_ projects: [String]) -> String {
    "Only the first 50 sessions of \(projects.joined(separator: ", ")) were checked: the hub lists sessions 50 at a time in id order, so a waiting session there may be missing."
}
