import SwiftUI

/// The hub at a glance (hub #3287): which satellites are up and how many
/// agents run per provider and effort. Read-only; pull to refresh, and it
/// re-reads every 30 seconds while shown (on a backoff after a failure,
/// keeping the last dashboard with its age, #350).
struct DashboardView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.theme) private var theme

    var body: some View {
        NavigationStack {
            Group {
                if model.host == nil {
                    ContentUnavailableView(
                        "No RSI host", systemImage: "network.slash",
                        description: Text("Set the hub's Tailscale host name in Settings."))
                } else if let dashboard = model.dashboard {
                    DashboardList(
                        dashboard: dashboard, freshness: model.dashboardFreshness,
                        usage: model.providerUsage)
                } else if let error = model.dashboardError {
                    ErrorView(error: error, freshness: model.dashboardFreshness, every: AppModel.dashboardEvery) {
                        Task { await model.refreshDashboard() }
                    }
                } else {
                    ProgressView("Loading dashboard…")
                }
            }
            .themedList()
            .navigationTitle("Dashboard")
            .refreshable { await model.refreshDashboard() }
            .task(id: model.refreshKey) { await model.keepDashboardFresh() }
        }
    }
}

struct DashboardList: View {
    @Environment(\.theme) private var theme
    let dashboard: Dashboard
    /// Whether the latest refresh failed over this dashboard.
    var freshness = Freshness()
    /// The provider usage screen (#504, `GET /api/v2/providers`); nil hides its link.
    var usage: ProviderUsageStore?

    var body: some View {
        List {
            FreshnessRow(freshness: freshness, every: AppModel.dashboardEvery)
            if let usage {
                NavigationLink {
                    ProviderUsageView(store: usage)
                } label: {
                    Label("Provider usage", systemImage: "chart.bar.xaxis")
                }
                .themedRow()
            }
            Section {
                if dashboard.satellites.isEmpty {
                    Text("No satellites are paired with this hub.")
                        .font(.caption).foregroundStyle(theme.mutedText.color).themedRow()
                }
                ForEach(dashboard.satellites) { SatelliteRowView(satellite: $0).themedRow() }
                if dashboard.satellitesTruncated {
                    Text("More satellites than shown.")
                        .font(.caption).foregroundStyle(theme.mutedText.color).themedRow()
                }
            } header: {
                SectionHeader(
                    title: "Satellites",
                    detail: dashboard.satellites.isEmpty ? nil : dashboard.reachableSummary)
            }
            Section {
                if dashboard.agents.isEmpty {
                    Text("No agents are running on the hub.")
                        .font(.caption).foregroundStyle(theme.mutedText.color).themedRow()
                }
                ForEach(dashboard.providers) { ProviderRowView(provider: $0).themedRow() }
                if dashboard.agentsTruncated {
                    Text("Some smaller groups are not listed; the total counts them.")
                        .font(.caption).foregroundStyle(theme.mutedText.color).themedRow()
                }
                if let note = dashboard.agentsIncompleteNote {
                    Text(note).font(.caption).foregroundStyle(theme.warning.color).themedRow()
                }
            } header: {
                SectionHeader(title: "Running agents", detail: dashboard.agentsSummary)
            } footer: {
                Text("Updated \(localTime(dashboard.observedAt))")
                    .font(.caption2).foregroundStyle(theme.time.color)
            }
        }
    }
}

private struct SectionHeader: View {
    @Environment(\.theme) private var theme
    let title: String
    let detail: String?

    var body: some View {
        HStack {
            Text(title).foregroundStyle(theme.label.color)
            Spacer()
            if let detail {
                Text(detail).foregroundStyle(theme.mutedText.color)
            }
        }
    }
}

struct SatelliteRowView: View {
    @Environment(\.theme) private var theme
    let satellite: DashboardSatellite

    var body: some View {
        HStack(alignment: .top) {
            Image(systemName: satellite.reachable ? "circle.fill" : "circle")
                .font(.caption)
                .foregroundStyle((satellite.reachable ? theme.running : theme.error).color)
                .padding(.top, 4)
                .accessibilityLabel(satellite.reachable ? "Reachable" : "Unreachable")
            VStack(alignment: .leading, spacing: 2) {
                Text(satellite.label).font(.headline)
                Text(satellite.lastSeen())
                    .font(.caption).foregroundStyle(theme.time.color)
                if let build = satellite.shortBuild {
                    Text("Build \(build)")
                        .font(.caption.monospaced()).foregroundStyle(theme.mutedText.color)
                }
            }
            Spacer()
            if let agents = satellite.agentsText {
                Text(agents).font(.subheadline)
                    .foregroundStyle(theme.resolve(theme.textAccents.count).color)
            }
        }
    }
}

struct ProviderRowView: View {
    @Environment(\.theme) private var theme
    let provider: DashboardProvider

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(provider.provider).font(.headline)
                    .foregroundStyle(theme.model.color)
                Spacer()
                Text(agentCount(provider.running)).font(.subheadline)
                    .foregroundStyle(theme.resolve(theme.textAccents.count).color)
            }
            ForEach(provider.groups) { group in
                HStack {
                    Text(group.effortLabel)
                        .font(.caption)
                        .foregroundStyle(
                            (group.effort == nil ? theme.mutedText : theme.resolve(theme.textAccents.effort)).color)
                    Spacer()
                    Text("\(group.running)").font(.caption.monospacedDigit())
                        .foregroundStyle(theme.mutedText.color)
                }
                .accessibilityElement(children: .combine)
            }
        }
    }
}
