import SwiftUI

/// Every provider's plan windows, reset countdowns, overage status and
/// recent spend (#504, `GET /api/v2/providers`). Read-only; it reads when opened, on pull-to-refresh
/// and every minute while shown (on a backoff after a failure, keeping the last
/// data with its age, #350). A provider with nothing to show says so.
struct ProviderUsageView: View {
    @Environment(\.theme) private var theme
    let store: ProviderUsageStore

    var body: some View {
        Group {
            if let usage = store.usage {
                ProviderUsageList(usage: usage, freshness: store.freshness)
            } else if let error = store.error {
                ErrorView(error: error, freshness: store.freshness, every: ProviderUsageStore.every) {
                    Task { await store.refresh() }
                }
            } else {
                ProgressView("Loading provider usage…")
            }
        }
        .themedList()
        .navigationTitle("Provider usage")
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await store.refresh() }
        .task { await store.keepFresh() }
    }
}

struct ProviderUsageList: View {
    @Environment(\.theme) private var theme
    let usage: ProviderUsage
    var freshness = Freshness()

    var body: some View {
        // The countdowns tick without a read: the rows are rebuilt every 30 s.
        TimelineView(.periodic(from: .now, by: 30)) { context in
            List {
                FreshnessRow(freshness: freshness, every: ProviderUsageStore.every)
                if usage.providers.isEmpty {
                    Text("No providers are configured on this hub.")
                        .font(.caption).foregroundStyle(theme.mutedText.color).themedRow()
                }
                ForEach(usage.rows(now: context.date)) { ProviderSection(row: $0) }
                if let note = usage.incompleteNote {
                    Text(note).font(.caption).foregroundStyle(theme.warning.color).themedRow()
                }
                if usage.observedAt != nil || !usage.providers.isEmpty {
                    Text(footer).font(.caption2).foregroundStyle(theme.time.color)
                        .listRowBackground(Color.clear)
                }
            }
        }
    }

    private var footer: String {
        var parts = [usage.summary]
        if let observed = usage.observedAt { parts.append("Updated \(localTime(observed))") }
        return parts.joined(separator: " · ")
    }
}

/// One provider: its name and verdict, then its windows, overage and spend.
private struct ProviderSection: View {
    @Environment(\.theme) private var theme
    let row: ProviderUsageRow

    var body: some View {
        Section {
            if !row.hasData {
                Text(row.noDataText)
                    .font(.subheadline).foregroundStyle(theme.mutedText.color).themedRow()
            }
            ForEach(row.windows) { WindowRowView(window: $0).themedRow() }
            if let overage = row.overage {
                DetailLine(symbol: "arrow.up.right.circle", text: overage, tone: row.overageTone).themedRow()
            }
            if let spend = row.spend {
                DetailLine(symbol: "dollarsign.circle", text: spend, tone: .idle).themedRow()
            }
            if let label = row.spendLabel {
                Text(label).font(.caption2).foregroundStyle(theme.mutedText.color).themedRow()
            }
            ForEach(row.modelSpend) { model in
                HStack(alignment: .firstTextBaseline) {
                    Text(model.model).font(.caption.monospaced()).foregroundStyle(theme.model.color)
                    Spacer()
                    Text(model.text).font(.caption).foregroundStyle(theme.mutedText.color)
                }
                .accessibilityElement(children: .combine)
                .themedRow()
            }
            ForEach(row.reasonNotes, id: \.self) { note in
                Text(note).font(.caption).foregroundStyle(theme.mutedText.color).themedRow()
            }
            if let note = row.staleNote {
                Label(note, systemImage: "clock.arrow.circlepath")
                    .font(.caption).foregroundStyle(theme.warning.color).themedRow()
            }
        } header: {
            HStack {
                Text(row.name).foregroundStyle(theme.label.color)
                Spacer()
                if let status = row.status {
                    Text(status).foregroundStyle(theme.color(row.statusTone))
                }
            }
        }
    }
}

private struct WindowRowView: View {
    @Environment(\.theme) private var theme
    let window: UsageWindowRow

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline) {
                Text(window.label).font(.headline)
                if window.isBinding {
                    Text("limiting").font(.caption2).foregroundStyle(theme.onAccent.color)
                        .padding(.horizontal, 6).padding(.vertical, 1)
                        .background(theme.mutedText.color, in: Capsule())
                }
                Spacer()
                Text(window.percentText).font(.headline.monospacedDigit())
                    .foregroundStyle(window.isExpired ? theme.mutedText.color : theme.color(window.tone))
            }
            UsageBar(fraction: window.fraction, tone: window.isExpired ? .idle : window.tone)
            Text(window.resetText).font(.caption).foregroundStyle(theme.time.color)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(window.label) window")
        .accessibilityValue("\(window.percentText). \(window.resetText)")
    }
}

/// The utilisation bar: a track and a fill in the tone's colour.
struct UsageBar: View {
    @Environment(\.theme) private var theme
    let fraction: Double?
    let tone: UsageTone

    var body: some View {
        GeometryReader { proxy in
            ZStack(alignment: .leading) {
                Capsule().fill(theme.palette.surface1.color)
                if let fraction {
                    Capsule().fill(theme.color(tone))
                        .frame(width: max(fraction > 0 ? 4 : 0, proxy.size.width * fraction))
                }
            }
        }
        .frame(height: 8)
        .accessibilityHidden(true)
    }
}

private struct DetailLine: View {
    @Environment(\.theme) private var theme
    let symbol: String
    let text: String
    let tone: UsageTone

    var body: some View {
        Label {
            Text(text).font(.subheadline).foregroundStyle(tone == .idle ? theme.primaryText.color : theme.color(tone))
        } icon: {
            Image(systemName: symbol).foregroundStyle(theme.mutedText.color)
        }
    }
}

extension RemoteTheme {
    /// A usage tone's colour in this theme.
    func color(_ tone: UsageTone) -> Color {
        switch tone {
        case .idle: mutedText.color
        case .ok: running.color
        case .warn: warning.color
        case .error: error.color
        }
    }
}

/// The offline fixture screen: `-providerUsageFixture full|credits|sparse|empty|failing`
/// opens it as the root, so a phone or simulator that cannot be tapped can be
/// screenshotted (#504). It touches no saved host and makes no request.
struct ProviderUsageDemoView: View {
    @Environment(AppModel.self) private var model

    static func isRequested(_ defaults: UserDefaults = .standard) -> Bool {
        ProviderUsageSource.current(defaults).isFixture
    }

    var body: some View {
        NavigationStack { ProviderUsageView(store: model.providerUsage) }
    }
}
