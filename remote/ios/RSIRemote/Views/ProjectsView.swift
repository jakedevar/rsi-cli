import SwiftUI

struct ProjectsView: View {
    @Environment(AppModel.self) private var model
    @State private var path: [String] = []
    @State private var openedForLaunch = false

    var body: some View {
        NavigationStack(path: $path) {
            Group {
                if model.host == nil {
                    ContentUnavailableView(
                        "No RSI host",
                        systemImage: "network.slash",
                        description: Text("Set the hub's Tailscale host name in Settings."))
                } else if let error = model.projectsError, model.projects.isEmpty {
                    ErrorView(error: error, freshness: model.projectsFreshness, every: AppModel.projectsEvery) {
                        Task { await model.refreshProjects() }
                    }
                } else if model.projects.isEmpty && model.loadingProjects {
                    ProgressView("Loading projects…")
                } else if model.projects.isEmpty {
                    ContentUnavailableView(
                        "No projects exposed",
                        systemImage: "folder",
                        description: Text("Choose projects in the hub's Settings → Integrations → Remote."))
                } else {
                    List {
                        FreshnessRow(freshness: model.projectsFreshness, every: AppModel.projectsEvery)
                        ForEach(model.projects) { row in
                            NavigationLink(value: row.id) {
                                ProjectRowView(row: row)
                            }
                            .themedRow()
                        }
                    }
                    .navigationDestination(for: String.self) { id in
                        ProjectView(projectId: id)
                    }
                }
            }
            .themedList()
            .navigationTitle("Projects")
            .refreshable { await model.refreshProjects() }
            // Reads on appear unless fresh, then every minute (on a backoff
            // after a failure) while shown; again on return to the foreground.
            .task(id: model.refreshKey) { await model.keepProjectsFresh() }
            // `-outboxOpenChat 1`: open the first manager chat once, to look at the outbox on a
            // phone that cannot be tapped from the Mac.
            .onChange(of: model.projects) { _, _ in
                guard !openedForLaunch, let id = model.launchChatProject() else { return }
                openedForLaunch = true
                path = [id]
            }
        }
    }
}

struct ProjectRowView: View {
    @Environment(\.theme) private var theme
    let row: ProjectRow

    var body: some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text(row.project.name).font(.headline)
                if let error = row.error {
                    Text(error).font(.caption).foregroundStyle(theme.error.color)
                } else {
                    let running = row.sessions.filter { $0.status.knownValue == "Running" }.count
                    let runningText = Text("\(running) running")
                        .foregroundStyle((running > 0 ? theme.running : theme.mutedText).color)
                    Text("\(row.sessions.count)\(row.partial ? "+" : "") sessions · \(runningText)")
                        .font(.caption).foregroundStyle(theme.mutedText.color)
                }
                if let seats = row.seats {
                    SeatLine(label: "PM", seat: seats.projectManager)
                    SeatLine(label: "Global", seat: seats.globalManager)
                }
            }
            Spacer()
            if let badge = row.badge {
                PendingBadge(text: badge)
                    .accessibilityLabel(row.seats != nil ? "\(badge) pending decisions" : "\(badge) waiting")
            }
        }
    }
}

/// `PM: Running · Claude claude-opus-5-5`, or that no seat is appointed.
struct SeatLine: View {
    @Environment(\.theme) private var theme
    let label: String
    let seat: ManagerSeat?

    var body: some View {
        Text("\(label): \(seat?.summary ?? "none appointed")")
            .font(.caption2).foregroundStyle(theme.resolve(theme.textAccents.model).color).lineLimit(1)
    }
}

/// One project: its manager (gateway v2) and its sessions, as last read.
struct ProjectView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.theme) private var theme
    let projectId: String
    @State private var pane = Pane.manager

    enum Pane: String, CaseIterable { case manager = "Manager", sessions = "Sessions" }

    var body: some View {
        if let row = model.project(projectId) {
            content(row)
        } else {
            ContentUnavailableView(
                "Project not listed", systemImage: "folder.badge.questionmark",
                description: Text("The hub no longer lists this project."))
        }
    }

    private func content(_ row: ProjectRow) -> some View {
        VStack(spacing: 0) {
            Picker("View", selection: $pane) {
                ForEach(Pane.allCases, id: \.self) { Text($0.rawValue) }
            }
            .pickerStyle(.segmented)
            .padding()
            SpectrumStrip(theme: theme, height: 2).padding(.horizontal)
            switch pane {
            case .manager:
                ManagerChatView(project: row.project)
            case .sessions:
                List {
                    FreshnessRow(freshness: model.projectsFreshness, every: AppModel.projectsEvery)
                    if let error = row.error {
                        Text(error).font(.caption).foregroundStyle(theme.warning.color).themedRow()
                    }
                    ForEach(row.sessions) { session in
                        NavigationLink {
                            SessionView(project: row.project, summary: session)
                        } label: {
                            SessionRowView(session: session)
                        }
                        .themedRow()
                    }
                }
                .themedList()
                .refreshable { await model.refreshProjects() }
            }
        }
        .themedList()
        .navigationTitle(row.project.name)
        .navigationBarTitleDisplayMode(.inline)
    }
}

struct SessionRowView: View {
    @Environment(\.theme) private var theme
    let session: SessionSummary

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(session.ownTitle.isEmpty ? "Untitled session" : session.ownTitle)
                    .font(.subheadline).lineLimit(2)
                if session.needsYou {
                    Image(systemName: "exclamationmark.bubble.fill")
                        .foregroundStyle(theme.waiting.color)
                        .accessibilityLabel("Waiting on you")
                }
            }
            let status = Text(session.status.display)
                .foregroundStyle(theme.status(session.status.knownValue ?? "").color)
            Text("\(status) · \(session.provider.display)")
                .font(.caption).foregroundStyle(theme.mutedText.color)
        }
    }
}

/// A screen with nothing to show yet because its read failed: what failed,
/// in plain words, when it tries again by itself, and a Retry button.
struct ErrorView: View {
    let error: RemoteError
    /// The screen's freshness, for the retry countdown; nil when it does not retry by itself.
    var freshness: Freshness? = nil
    var every: Duration = .seconds(60)
    let retry: () -> Void

    var body: some View {
        ContentUnavailableView {
            Label(error.headline, systemImage: error.symbol)
        } description: {
            if let freshness {
                TimelineView(.periodic(from: .now, by: 1)) { context in
                    Text("\(error.message)\n\(freshness.retryText(now: context.date, every: every).capitalizedFirst).")
                }
            } else {
                Text(error.message)
            }
        } actions: {
            Button("Retry now", action: retry)
        }
    }
}

/// The first row of a list whose latest refresh failed: what failed, the age
/// of the data shown under it and when the screen reads again. Nothing while
/// the latest refresh went well.
struct FreshnessRow: View {
    @Environment(\.theme) private var theme
    let freshness: Freshness
    let every: Duration

    var body: some View {
        if let error = freshness.error {
            TimelineView(.periodic(from: .now, by: 1)) { context in
                Label(freshness.status(now: context.date, every: every) ?? error.headline, systemImage: error.symbol)
                    .font(.caption)
                    .foregroundStyle(theme.warning.color)
                    .accessibilityElement(children: .combine)
            }
            .themedRow()
        }
    }
}

extension RemoteError {
    /// The SF Symbol for this kind of failure.
    var symbol: String {
        switch self {
        case .unreachable: "wifi.exclamationmark"
        case .busy, .daemonUnavailable: "hourglass"
        case .forbidden, .unauthorized: "lock"
        default: "exclamationmark.triangle"
        }
    }
}

extension String {
    /// The string with its first letter upper-cased.
    var capitalizedFirst: String { prefix(1).uppercased() + dropFirst() }
}
