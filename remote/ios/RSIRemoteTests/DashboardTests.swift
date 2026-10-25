import Foundation
import XCTest

@testable import RSIRemote

// `GET /api/v2/dashboard` (hub #3287): the fixtures decode into what the
// Dashboard screen shows, with last-seen times in the phone's local time.

final class DashboardTests: XCTestCase {
    private let pacific = TimeZone(identifier: "America/Los_Angeles")!
    private let us = Locale(identifier: "en_US")

    private func dashboard(_ name: String) throws -> Dashboard {
        try V2Fixture.named(name).decode(Dashboard.self)
    }

    func testThePopulatedFixtureShowsSatellitesAndTheirReports() throws {
        let board = try dashboard("dashboard_200")
        XCTAssertEqual(board.satellites.map(\.label), ["laptop", "mac-mini"])
        XCTAssertEqual(board.reachableSummary, "1 of 2 reachable")
        let laptop = board.satellites[0]
        XCTAssertFalse(laptop.reachable)
        XCTAssertNil(laptop.shortBuild)
        XCTAssertNil(laptop.agentsText)
        let mini = board.satellites[1]
        XCTAssertTrue(mini.reachable)
        XCTAssertEqual(mini.shortBuild, "a077ca3b0")
        XCTAssertEqual(mini.agentsText, "3 agents")
        let now = parseWireTime("2026-10-19T20:00:00Z")!
        XCTAssertEqual(mini.lastSeen(now: now, timeZone: pacific, locale: us), "Last seen 12:29\u{202F}PM")
        XCTAssertFalse(board.satellitesTruncated)
    }

    func testAgentsGroupByProviderWithEffortOrNone() throws {
        let board = try dashboard("dashboard_200")
        XCTAssertEqual(board.agentsTotal, 7)
        XCTAssertFalse(board.agentsTruncated)
        XCTAssertFalse(board.agentsIncomplete)
        XCTAssertEqual(board.agentsSummary, "7 on the hub")
        XCTAssertNil(board.agentsIncompleteNote)
        let providers = board.providers
        XCTAssertEqual(providers.map(\.provider), ["Claude", "Codex"])
        XCTAssertEqual(providers.map(\.running), [5, 2])
        XCTAssertEqual(providers[0].groups.map(\.effortLabel), ["high", "no effort set"])
        XCTAssertEqual(providers[0].groups.map(\.running), [4, 1])
        XCTAssertEqual(providers[1].groups.map(\.effortLabel), ["xhigh"])
        XCTAssertEqual(Set(board.agents.map(\.id)).count, board.agents.count, "group ids are unique")
    }

    func testTheEmptyFixtureHasNothingRunning() throws {
        let board = try dashboard("dashboard_empty_200")
        XCTAssertEqual(board.satellites, [])
        XCTAssertEqual(board.agents, [])
        XCTAssertEqual(board.providers, [])
        XCTAssertEqual(board.agentsTotal, 0)
    }

    func testAHubPastItsAgentCapSaysMoreThan() throws {
        let board = try dashboard("dashboard_incomplete_200")
        XCTAssertTrue(board.agentsIncomplete)
        XCTAssertEqual(board.agentsTotal, 1024)
        XCTAssertEqual(board.agentsSummary, "More than 1024 on the hub")
        XCTAssertEqual(
            board.agentsIncompleteNote,
            "More than 1024 agents are running; the counts cover the first 1024.")
        XCTAssertEqual(board.providers.map(\.running), [1000, 24])
    }

    func testANeverSeenSatelliteSaysSo() throws {
        let satellite = try JSONDecoder().decode(
            DashboardSatellite.self,
            from: Data(
                #"{"label":"new","reachable":false,"build_sha":null,"last_seen_at":null,"running_agents":null}"#
                    .utf8))
        XCTAssertEqual(satellite.lastSeen(), "Never seen")
        XCTAssertEqual(agentCount(1), "1 agent")
    }

    func testTheClientReadsTheDashboardRoute() async throws {
        let fixture = try V2Fixture.named("dashboard_200")
        StubGateway.shared.reset()
        StubGateway.shared.script("GET /auth/bootstrap", .json(#"{"nonce":"n-1"}"#))
        StubGateway.shared.script("POST /auth/session", .json(#"{"csrf":"csrf-1"}"#))
        StubGateway.shared.script("GET /api/v2/dashboard", fixture.reply)
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubProtocol.self]
        let client = RemoteClient(host: V2Fixture.host, session: URLSession(configuration: configuration))
        let board = try await client.dashboard()
        XCTAssertEqual(board, try fixture.decode(Dashboard.self))
        let sent = StubGateway.shared.requests.filter { $0.path == "/api/v2/dashboard" }
        XCTAssertEqual(sent.count, 1)
        XCTAssertNil(sent.first?.query)
        XCTAssertEqual(sent.first?.headers["origin"], "https://\(V2Fixture.host)")
    }
}
