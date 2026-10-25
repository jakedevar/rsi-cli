import Foundation

// `GET /api/v2/dashboard` (hub #3287, docs/rsi-remote/contracts-v2.md): the
// satellites the hub polls and its running agents per provider and effort,
// labels and counts only. Wire types `rsi_common::remote_manager_v2::Dashboard*`.

/// `DashboardSatelliteV2`: one polled satellite.
struct DashboardSatellite: Decodable, Sendable, Hashable, Identifiable {
    let label: String
    let reachable: Bool
    /// Null until the satellite has reported since the hub started.
    let buildSha: String?
    /// The hub's last good poll, null if never.
    let lastSeenAt: String?
    /// Running plus waiting-approval sessions it last reported; null until reported.
    let runningAgents: Int?

    var id: String { label }

    enum CodingKeys: String, CodingKey {
        case label, reachable
        case buildSha = "build_sha"
        case lastSeenAt = "last_seen_at"
        case runningAgents = "running_agents"
    }

    /// The first 9 hex characters, as `git log --oneline` shows them.
    var shortBuild: String? { buildSha.map { String($0.prefix(9)) } }

    /// `Last seen 12:04`, or that the hub has never reached it.
    func lastSeen(now: Date = Date(), timeZone: TimeZone = .current, locale: Locale = .current) -> String {
        guard let lastSeenAt else { return "Never seen" }
        return "Last seen \(localTime(lastSeenAt, now: now, timeZone: timeZone, locale: locale))"
    }

    /// `3 agents`, or nil before the satellite has reported.
    var agentsText: String? { runningAgents.map(agentCount) }
}

/// `DashboardAgentGroupV2`: the hub's running agents of one provider at one effort.
struct DashboardAgentGroup: Decodable, Sendable, Hashable, Identifiable {
    let provider: String
    /// Null when the sessions have no effort set.
    let effort: String?
    let running: Int

    var id: String { "\(provider)|\(effort ?? "")" }

    /// The effort, or that none is set.
    var effortLabel: String { effort ?? "no effort set" }
}

/// One provider's running agents across its effort levels.
struct DashboardProvider: Sendable, Hashable, Identifiable {
    let provider: String
    let groups: [DashboardAgentGroup]
    var id: String { provider }
    var running: Int { groups.reduce(0) { $0 + $1.running } }
}

/// `DashboardResponseV2`.
struct Dashboard: Decodable, Sendable, Hashable {
    let observedAt: String
    let satellites: [DashboardSatellite]
    let satellitesTruncated: Bool
    /// Largest group first.
    let agents: [DashboardAgentGroup]
    /// Every running agent the hub counted, including groups the group cap
    /// cut. Exactly the hub's agent cap when `agentsIncomplete` is true.
    let agentsTotal: Int
    let agentsTruncated: Bool
    /// More agents are running than the hub counts: `agentsTotal` and every
    /// group count are lower bounds.
    let agentsIncomplete: Bool

    enum CodingKeys: String, CodingKey {
        case observedAt = "observed_at"
        case satellites
        case satellitesTruncated = "satellites_truncated"
        case agents
        case agentsTotal = "agents_total"
        case agentsTruncated = "agents_truncated"
        case agentsIncomplete = "agents_incomplete"
    }

    /// `7 on the hub`, or `More than 1024 on the hub` past the hub's cap.
    var agentsSummary: String {
        agentsIncomplete ? "More than \(agentsTotal) on the hub" : "\(agentsTotal) on the hub"
    }

    /// The note under the groups when the hub stopped counting at its cap.
    var agentsIncompleteNote: String? {
        agentsIncomplete
            ? "More than \(agentsTotal) agents are running; the counts cover the first \(agentsTotal)."
            : nil
    }

    /// The groups per provider, the busiest provider first (ties by name);
    /// each provider's efforts keep the daemon's order (largest first).
    var providers: [DashboardProvider] {
        var order: [String] = []
        var byProvider: [String: [DashboardAgentGroup]] = [:]
        for group in agents {
            if byProvider[group.provider] == nil { order.append(group.provider) }
            byProvider[group.provider, default: []].append(group)
        }
        return order.map { DashboardProvider(provider: $0, groups: byProvider[$0] ?? []) }
            .sorted { $0.running != $1.running ? $0.running > $1.running : $0.provider < $1.provider }
    }

    /// `2 of 3 reachable`.
    var reachableSummary: String {
        "\(satellites.filter(\.reachable).count) of \(satellites.count) reachable"
    }
}

/// `1 agent`, `3 agents`.
func agentCount(_ count: Int) -> String {
    count == 1 ? "1 agent" : "\(count) agents"
}
