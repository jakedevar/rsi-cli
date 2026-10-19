import SwiftUI

/// Every session that waits on the operator, across the exposed projects.
struct WaitingView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        NavigationStack {
            Group {
                if model.host == nil {
                    ContentUnavailableView(
                        "No RSI host", systemImage: "network.slash",
                        description: Text("Set the hub's Tailscale host name in Settings."))
                } else if let error = model.waitingError, model.waiting.isEmpty {
                    ErrorView(error: error) { Task { await model.refreshWaiting() } }
                } else if model.waiting.isEmpty && model.loadingWaiting {
                    ProgressView("Checking for decisions…")
                } else if model.waiting.isEmpty {
                    ContentUnavailableView(
                        "Nothing waiting", systemImage: "checkmark.circle",
                        description: Text("No manager or session is waiting on you."))
                } else {
                    List(model.waiting) { item in
                        Section {
                            if let error = item.error {
                                Text(error).font(.caption).foregroundStyle(.red)
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
                        }
                    }
                }
            }
            .navigationTitle("Waiting")
            .refreshable { await model.refreshWaiting() }
            .task(id: model.host) { await model.refreshWaiting() }
        }
    }
}
