import Foundation
import XCTest

@testable import RSIRemote

// #350: every screen keeps its last good data with its age, names what
// failed, reads again by itself on a backoff, and treats a cancelled read
// (a tab switch, a navigation) as nothing.

// MARK: Freshness

final class FreshnessTests: XCTestCase {
    private let start = Date(timeIntervalSince1970: 1_000_000)

    func testTheBackoffDoublesFromFiveSecondsToTwoMinutes() {
        var freshness = Freshness()
        var schedule: [Duration] = []
        for _ in 0..<7 {
            freshness.failed(.busy, at: start)
            schedule.append(freshness.backoff)
        }
        XCTAssertEqual(schedule, [5, 10, 20, 40, 80, 120, 120].map { Duration.seconds($0) })
        freshness.failed(.busy, at: start, retryAfter: 300)
        XCTAssertEqual(freshness.backoff, .seconds(300))
        freshness.succeeded(at: start)
        XCTAssertNil(freshness.error)
        XCTAssertEqual(freshness.failures, 0)
    }

    func testACancelledRefreshChangesNothing() {
        var freshness = Freshness()
        freshness.succeeded(at: start)
        freshness.failed(.cancelled, at: start.addingTimeInterval(5))
        XCTAssertNil(freshness.error)
        XCTAssertEqual(freshness.failures, 0)
    }

    func testTheStatusLineNamesTheFailureTheAgeAndTheRetry() {
        var freshness = Freshness()
        freshness.succeeded(at: start)
        XCTAssertNil(freshness.status(now: start.addingTimeInterval(30), every: .seconds(60)))
        XCTAssertEqual(freshness.status(now: start.addingTimeInterval(150), every: .seconds(60)), "Updated 2 min ago")
        freshness.failed(.busy, at: start.addingTimeInterval(120))
        XCTAssertEqual(
            freshness.status(now: start.addingTimeInterval(122), every: .seconds(60)),
            "Hub busy · showing data from 2 min ago · retrying in 3 s")
        XCTAssertEqual(
            freshness.status(now: start.addingTimeInterval(126), every: .seconds(60)),
            "Hub busy · showing data from 2 min ago · retrying now")
        var never = Freshness()
        never.failed(.daemonUnavailable, at: start)
        XCTAssertEqual(never.status(now: start, every: .seconds(60)), "Hub restarting · retrying in 5 s")
    }

    func testTheWaitFollowsTheSuccessPeriodOrTheBackoff() {
        var freshness = Freshness()
        XCTAssertEqual(freshness.wait(every: .seconds(60), now: start), .seconds(1))
        freshness.succeeded(at: start)
        XCTAssertEqual(freshness.wait(every: .seconds(60), now: start.addingTimeInterval(20)), .seconds(40))
        XCTAssertFalse(freshness.isStale(15, now: start.addingTimeInterval(10)))
        XCTAssertTrue(freshness.isStale(15, now: start.addingTimeInterval(20)))
        freshness.failed(.busy, at: start.addingTimeInterval(20))
        XCTAssertEqual(freshness.wait(every: .seconds(60), now: start.addingTimeInterval(21)), .seconds(4))
        XCTAssertTrue(freshness.isStale(15, now: start.addingTimeInterval(21)), "a failed refresh is stale")
    }

    func testAgesReadNaturally() {
        XCTAssertEqual(ageText(3), "just now")
        XCTAssertEqual(ageText(45), "45 s ago")
        XCTAssertEqual(ageText(125), "2 min ago")
        XCTAssertEqual(ageText(7200), "2 h ago")
        XCTAssertEqual(ageText(200_000), "2 d ago")
    }
}

// MARK: Screens

@MainActor
final class ScreenReliabilityTests: XCTestCase {
    private final class Clock {
        var date = Date(timeIntervalSince1970: 1_000_000)
    }

    private let clock = Clock()

    override func setUp() {
        super.setUp()
        StubGateway.shared.reset()
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v2/projects", .json(#"{"projects":[],"messages_enabled":true}"#))
    }

    private func makeModel() throws -> AppModel {
        let suite = "rsi-remote-tests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        addTeardownBlock { defaults.removePersistentDomain(forName: suite) }
        let model = AppModel(defaults: defaults) { _ in RemoteStub.client() }
        let clock = clock
        model.now = { clock.date }
        XCTAssertTrue(model.setHost(RemoteStub.host))
        return model
    }

    func testAFailedRefreshKeepsTheProjectsAndSaysHowOldTheyAre() async throws {
        StubGateway.shared.script("GET /api/v1/projects", .json(RemoteStub.projectsBody(["p1"])))
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))", .json(RemoteStub.sessionsBody("p1", waiting: [true, false])))
        let model = try makeModel()
        await model.refreshProjects()
        XCTAssertEqual(model.projects.first?.sessions.count, 2)
        XCTAssertNil(model.projectsStatus)

        clock.date += 120
        StubGateway.shared.script("GET /api/v1/projects", RemoteStub.busy)
        await model.refreshProjects()
        XCTAssertEqual(model.projects.first?.sessions.count, 2, "the last good data stays")
        XCTAssertEqual(model.projectsError, .busy)
        XCTAssertEqual(model.projectsStatus, "Hub busy · showing data from 2 min ago · retrying in 5 s")
        XCTAssertEqual(model.waitingCount, 1)
    }

    func testAProjectWhoseSessionsCouldNotBeReadKeepsItsLastSessions() async throws {
        StubGateway.shared.script("GET /api/v1/projects", .json(RemoteStub.projectsBody(["p1", "p2"])))
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))", .json(RemoteStub.sessionsBody("p1", waiting: [true])))
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p2"))", .json(RemoteStub.sessionsBody("p2", waiting: [false])))
        let model = try makeModel()
        await model.refreshProjects()
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))", RemoteStub.busy)
        await model.refreshProjects()
        let row = try XCTUnwrap(model.project("p1"))
        XCTAssertEqual(row.sessions.count, 1)
        XCTAssertEqual(row.waiting, 1)
        XCTAssertEqual(row.error, "Hub busy · sessions not refreshed")
        XCTAssertNil(model.project("p2")?.error)
    }

    func testACancelledRefreshChangesNothingOnScreen() async throws {
        StubGateway.shared.script("GET /api/v1/projects", .json(RemoteStub.projectsBody(["p1"])))
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))", .json(RemoteStub.sessionsBody("p1", waiting: [false])))
        let model = try makeModel()
        await model.refreshProjects()
        StubGateway.shared.script("GET /api/v1/projects", StubGateway.Reply.json(RemoteStub.projectsBody(["p1", "p2"])).after(5))
        let refresh = Task { await model.refreshProjects() }
        try await Task.sleep(for: .milliseconds(100))
        refresh.cancel()
        await refresh.value
        XCTAssertNil(model.projectsError, "a tab switch is not a failure")
        XCTAssertEqual(model.projects.map(\.id), ["p1"])
    }

    func testWaitingReadsTheSessionListsAgainAndKeepsDecisionsOnAFailure() async throws {
        StubGateway.shared.script("GET /api/v1/projects", .json(RemoteStub.projectsBody(["p1"])))
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))", .json(RemoteStub.sessionsBody("p1", waiting: [false])))
        let model = try makeModel()
        await model.refreshProjects()
        await model.refreshWaiting()
        XCTAssertTrue(model.waiting.isEmpty)

        // A session starts waiting; the Waiting screen finds it on its own.
        clock.date += 30
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))", .json(RemoteStub.sessionsBody("p1", waiting: [true])))
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))/p1-s0/decisions", .json(RemoteStub.decisionsBody))
        await model.refreshWaiting()
        XCTAssertEqual(model.waiting.map(\.id), ["p1-s0"])
        XCTAssertEqual(model.waiting.first?.decisions.count, 1)

        // The next decisions read is busy: the decision stays, marked not refreshed.
        clock.date += 30
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))/p1-s0/decisions", RemoteStub.busy)
        await model.refreshWaiting()
        XCTAssertEqual(model.waiting.first?.decisions.count, 1)
        XCTAssertEqual(model.waiting.first?.error, "Hub busy · decisions not refreshed")
    }

    func testAScreenThatReappearsSoonReusesItsData() async throws {
        StubGateway.shared.script("GET /api/v1/projects", .json(RemoteStub.projectsBody(["p1"])))
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p1"))", .json(RemoteStub.sessionsBody("p1", waiting: [false])))
        let model = try makeModel()
        await model.refreshProjects(ifOlderThan: AppModel.reuseFor)
        clock.date += 5
        await model.refreshProjects(ifOlderThan: AppModel.reuseFor)
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 1)
        clock.date += 20
        await model.refreshProjects(ifOlderThan: AppModel.reuseFor)
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 2)
    }

    func testAProjectWithMoreSessionsThanOnePageSaysSo() async throws {
        StubGateway.shared.script("GET /api/v1/projects", .json(RemoteStub.projectsBody(["p1", "p2"])))
        StubGateway.shared.script(
            "GET \(RemoteStub.sessionsPath("p1"))", .json(RemoteStub.sessionsBody("p1", waiting: [false], more: true)))
        StubGateway.shared.script("GET \(RemoteStub.sessionsPath("p2"))", .json(RemoteStub.sessionsBody("p2", waiting: [false])))
        let model = try makeModel()
        await model.refreshProjects()
        XCTAssertEqual(model.project("p1")?.partial, true)
        XCTAssertEqual(model.project("p2")?.partial, false)
        XCTAssertEqual(model.partlyCheckedProjects, ["Project p1"])
        XCTAssertTrue(partlyCheckedText(model.partlyCheckedProjects).contains("Only the first 50 sessions of Project p1"))
    }

    func testTheForegroundRestartsTheShownScreensLoops() throws {
        let model = try makeModel()
        let before = model.refreshKey
        model.foregrounded()
        XCTAssertNotEqual(model.refreshKey, before)
    }
}
