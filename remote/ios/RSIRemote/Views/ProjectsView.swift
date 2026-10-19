import SwiftUI

struct ProjectsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        NavigationStack {
            Group {
                if model.host == nil {
                    ContentUnavailableView(
                        "No RSI host",
                        systemImage: "network.slash",
                        description: Text("Set the hub's Tailscale host name in Settings."))
                } else if let error = model.projectsError, model.projects.isEmpty {
                    ErrorView(error: error) { Task { await model.refreshProjects() } }
                } else if model.projects.isEmpty && model.loadingProjects {
                    ProgressView("Loading projects…")
                } else if model.projects.isEmpty {
                    ContentUnavailableView(
                        "No projects exposed",
                        systemImage: "folder",
                        description: Text("Choose projects in the hub's Settings → Integrations → Remote."))
                } else {
                    List(model.projects) { row in
                        NavigationLink(value: row) {
                            ProjectRowView(row: row)
                        }
                    }
                    .navigationDestination(for: ProjectRow.self) { row in
                        ProjectView(row: row)
                    }
                }
            }
            .navigationTitle("Projects")
            .refreshable { await model.refreshProjects() }
            .task(id: model.host) { await model.refreshProjects() }
        }
    }
}

struct ProjectRowView: View {
    let row: ProjectRow

    var body: some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text(row.project.name).font(.headline)
                if let error = row.error {
                    Text(error).font(.caption).foregroundStyle(.red)
                } else {
                    let running = row.sessions.filter { $0.status.knownValue == "Running" }.count
                    Text("\(row.sessions.count) sessions · \(running) running")
                        .font(.caption).foregroundStyle(.secondary)
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
    let label: String
    let seat: ManagerSeat?

    var body: some View {
        Text("\(label): \(seat?.summary ?? "none appointed")")
            .font(.caption2).foregroundStyle(.secondary).lineLimit(1)
    }
}

/// One project: its manager (gateway v2) and its sessions.
struct ProjectView: View {
    let row: ProjectRow
    @State private var pane = Pane.manager

    enum Pane: String, CaseIterable { case manager = "Manager", sessions = "Sessions" }

    var body: some View {
        VStack(spacing: 0) {
            Picker("View", selection: $pane) {
                ForEach(Pane.allCases, id: \.self) { Text($0.rawValue) }
            }
            .pickerStyle(.segmented)
            .padding()
            switch pane {
            case .manager:
                ManagerChatView(project: row.project)
            case .sessions:
                List(row.sessions) { session in
                    NavigationLink {
                        SessionView(project: row.project, summary: session)
                    } label: {
                        SessionRowView(session: session)
                    }
                }
            }
        }
        .navigationTitle(row.project.name)
        .navigationBarTitleDisplayMode(.inline)
    }
}

struct SessionRowView: View {
    let session: SessionSummary

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(session.ownTitle.isEmpty ? "Untitled session" : session.ownTitle)
                    .font(.subheadline).lineLimit(2)
                if session.needsYou {
                    Image(systemName: "exclamationmark.bubble.fill")
                        .foregroundStyle(.orange)
                        .accessibilityLabel("Waiting on you")
                }
            }
            Text("\(session.status.display) · \(session.provider.display)")
                .font(.caption).foregroundStyle(.secondary)
        }
    }
}

struct ErrorView: View {
    let error: RemoteError
    let retry: () -> Void

    var body: some View {
        ContentUnavailableView {
            Label(error.isUnreachable ? "Can't reach RSI" : "RSI error",
                systemImage: error.isUnreachable ? "wifi.exclamationmark" : "exclamationmark.triangle")
        } description: {
            Text(error.message)
        } actions: {
            Button("Retry", action: retry)
        }
    }
}
