import Foundation
import XCTest

@testable import RSIRemote

// #350: the app rides out a busy, restarting or briefly unreachable hub. Reads
// share one sign-in, stay under the gateway's permits, retry what is
// transient, and every screen keeps its last good data with its age.

/// Stub-gateway helpers shared by the reliability tests.
enum RemoteStub {
    static let host = "hub.example.ts.net"

    static func client(retry: ReadRetry = .instant) -> RemoteClient {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubProtocol.self]
        return RemoteClient(host: host, session: URLSession(configuration: configuration), retry: retry)
    }

    /// The sign-in routes; `delay` keeps the bootstrap in flight a little.
    static func scriptSignIn(delay: Double = 0) {
        StubGateway.shared.script("GET /auth/bootstrap", StubGateway.Reply.json(#"{"nonce":"n-1"}"#).after(delay))
        StubGateway.shared.script(
            "POST /auth/session", .json(#"{"csrf":"csrf-1"}"#), .json(#"{"csrf":"csrf-2"}"#),
            .json(#"{"csrf":"csrf-3"}"#))
    }

    static func signIns() -> (bootstraps: Int, sessions: Int) {
        let requests = StubGateway.shared.requests
        return (
            requests.filter { $0.path == "/auth/bootstrap" }.count,
            requests.filter { $0.path == "/auth/session" }.count
        )
    }

    static func count(_ path: String) -> Int {
        StubGateway.shared.requests.filter { $0.path == path }.count
    }

    static func sessionsPath(_ project: String) -> String { "/api/v1/projects/\(project)/sessions" }

    static func projectsBody(_ ids: [String]) -> String {
        let items = ids.map { #"{"id":"\#($0)","name":"Project \#($0)"}"# }.joined(separator: ",")
        return #"{"version":"1","observed_at":"2026-10-22T00:00:00.000000000Z","next_cursor":null,"complete":true,"degraded":[],"items":[\#(items)]}"#
    }

    /// One session per flag (true: it waits on the operator); `more` adds a next page.
    static func sessionsBody(_ project: String, waiting: [Bool], more: Bool = false) -> String {
        let items = waiting.enumerated().map { index, waits in
            """
            {"id":"\(project)-s\(index)","project_id":"\(project)","parent_id":null,"continued_from":null,\
            "kind":{"state":"known","value":"Worker"},"own_title":"Worker \(index)",\
            "provider":{"state":"known","value":"Claude"},"status":{"state":"known","value":"Running"},\
            "updated_at":"2026-10-22T00:00:00.000000000Z",\
            "attention":{"requires_local_action":\(waits),"incomplete":false}}
            """
        }.joined(separator: ",")
        let next = more ? #"{"kind":"sessions","token":"t"}"# : "null"
        return #"{"next_cursor":\#(next),"complete":true,"degraded":[],"project":{"id":"\#(project)","name":"P"},"items":[\#(items)]}"#
    }

    static let decisionsBody = """
        {"complete":true,"project_id":"p1","session_id":"p1-s0","items":[{"id":"d1","kind":"generic_questions",\
        "closure_state":"open","display":{"kind":"generic_questions"},"questions":[{"header":null,\
        "question":"Proceed?","options":[{"label":"Yes","description":null}],"multi_select":false}],\
        "requires_local_action":true}]}
        """

    static let busy = StubGateway.Reply.json(#"{"error":"busy"}"#, status: 503, headers: ["Retry-After": "2"])
}

/// Records the backoff waits instead of sleeping.
final class Sleeps: @unchecked Sendable {
    private let lock = NSLock()
    private var seen: [Double] = []

    var all: [Double] {
        lock.lock(); defer { lock.unlock() }
        return seen
    }

    var retry: ReadRetry {
        ReadRetry(jitter: { 1 }, sleep: { [self] seconds in record(seconds) })
    }

    private func record(_ seconds: Double) {
        lock.withLock { seen.append(seconds) }
    }
}

// MARK: Client

final class ReadReliabilityTests: XCTestCase {
    override func setUp() {
        super.setUp()
        StubGateway.shared.reset()
    }

    func testConcurrentReadsShareOneSignIn() async throws {
        // The gateway's boot cookie is single-use and the cookie jar holds one:
        // interleaved sign-ins refuse each other with a 403 (reproduced against
        // the gateway emulator, remote/ios/tools/reliability).
        RemoteStub.scriptSignIn(delay: 0.05)
        let projects = (1...6).map { "p\($0)" }
        for project in projects {
            StubGateway.shared.script("GET \(RemoteStub.sessionsPath(project))", .json(RemoteStub.sessionsBody(project, waiting: [false])))
        }
        let client = RemoteStub.client()
        try await withThrowingTaskGroup(of: Void.self) { group in
            for project in projects {
                group.addTask { _ = try await client.sessions(project: project) }
            }
            try await group.waitForAll()
        }
        XCTAssertEqual(RemoteStub.signIns().bootstraps, 1)
        XCTAssertEqual(RemoteStub.signIns().sessions, 1)
    }

    func testReadsThatLostTheirSessionSignInAgainOnlyOnce() async throws {
        RemoteStub.scriptSignIn()
        let client = RemoteStub.client()
        StubGateway.shared.script("GET /api/v1/projects", .json(RemoteStub.projectsBody(["p1"])))
        _ = try await client.projects()
        // The gateway restarted (a deploy) or the session idled out: every read gets a 401.
        let projects = (1...4).map { "p\($0)" }
        for project in projects {
            StubGateway.shared.script(
                "GET \(RemoteStub.sessionsPath(project))", StubGateway.Reply.json("unauthorized", status: 401).after(0.02),
                .json(RemoteStub.sessionsBody(project, waiting: [false])))
        }
        try await withThrowingTaskGroup(of: Void.self) { group in
            for project in projects {
                group.addTask { _ = try await client.sessions(project: project) }
            }
            try await group.waitForAll()
        }
        XCTAssertEqual(RemoteStub.signIns().bootstraps, 2, "one sign-in at start, one after the 401s")
        XCTAssertEqual(RemoteStub.signIns().sessions, 2)
    }

    func testRequestsInFlightStayUnderTheGatewayPermits() async throws {
        RemoteStub.scriptSignIn()
        let projects = (1...10).map { "p\($0)" }
        for project in projects {
            StubGateway.shared.script(
                "GET \(RemoteStub.sessionsPath(project))", StubGateway.Reply.json(RemoteStub.sessionsBody(project, waiting: [false])).after(0.03))
        }
        let board = try V2Fixture.named("dashboard_200").reply.after(0.03)
        StubGateway.shared.script("GET /api/v2/dashboard", board)
        let client = RemoteStub.client()
        try await withThrowingTaskGroup(of: Void.self) { group in
            for project in projects {
                group.addTask { _ = try await client.sessions(project: project) }
            }
            for _ in 0..<5 {
                group.addTask { _ = try await client.dashboard() }
            }
            try await group.waitForAll()
        }
        let peaks = StubGateway.shared.peaks
        XCTAssertLessThanOrEqual(peaks.all, RemoteClient.maxInFlight)
        XCTAssertLessThanOrEqual(peaks.operator, RemoteClient.maxOperatorInFlight)
        XCTAssertGreaterThan(peaks.all, 1, "reads still run side by side")
        XCTAssertEqual(RemoteStub.count("/api/v2/dashboard"), 5)
    }

    func testABusyReadIsTriedAgainAfterItsRetryAfter() async throws {
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v1/projects", RemoteStub.busy, .json(#"{"error":"busy"}"#, status: 503), .json(RemoteStub.projectsBody(["p1"])))
        let sleeps = Sleeps()
        let list = try await RemoteStub.client(retry: sleeps.retry).projects()
        XCTAssertEqual(list.items.map(\.id), ["p1"])
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 3)
        // Retry-After 2 wins over the first 0.5 s backoff; the second backs off 1 s.
        XCTAssertEqual(sleeps.all, [2, 1])
    }

    func testARestartingDaemonIsTriedUpToTheLimitThenNamedPlainly() async throws {
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v2/dashboard", .json(#"{"error":"connect"}"#, status: 502))
        let sleeps = Sleeps()
        do {
            _ = try await RemoteStub.client(retry: sleeps.retry).dashboard()
            XCTFail("expected the daemon to be unavailable")
        } catch {
            XCTAssertEqual(error as? RemoteError, .daemonUnavailable)
            XCTAssertEqual((error as? RemoteError)?.headline, "Hub restarting")
        }
        XCTAssertEqual(RemoteStub.count("/api/v2/dashboard"), ReadRetry.standard.attempts)
        XCTAssertEqual(sleeps.all, [0.5, 1, 2])
    }

    func testABare403IsTriedOnceMoreThenRefused() async throws {
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v1/projects", .json("forbidden", status: 403), .json(RemoteStub.projectsBody(["p1"])))
        let list = try await RemoteStub.client().projects()
        XCTAssertEqual(list.items.count, 1, "a passing identity-check refusal (localapi_busy) recovers")
        StubGateway.shared.reset()
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v1/projects", .json("forbidden", status: 403))
        do {
            _ = try await RemoteStub.client().projects()
            XCTFail("expected a refusal")
        } catch {
            XCTAssertEqual(error as? RemoteError, .forbidden)
        }
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 2)
    }

    func testASignInRefusedOnceIsTriedAgain() async throws {
        StubGateway.shared.script("GET /auth/bootstrap", .json("forbidden", status: 403), .json(#"{"nonce":"n-1"}"#))
        StubGateway.shared.script("POST /auth/session", .json(#"{"csrf":"csrf-1"}"#))
        StubGateway.shared.script("GET /api/v1/projects", .json(RemoteStub.projectsBody(["p1"])))
        let list = try await RemoteStub.client().projects()
        XCTAssertEqual(list.items.count, 1)
        XCTAssertEqual(RemoteStub.signIns().bootstraps, 2)
    }

    func testA500OrA404IsNotTriedAgainInPlace() async throws {
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v1/projects", .json(#"{"error":"daemon_error"}"#, status: 500))
        do {
            _ = try await RemoteStub.client().projects()
            XCTFail("expected a server error")
        } catch {
            XCTAssertEqual(error as? RemoteError, .server(status: 500, code: "daemon_error"))
        }
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 1)
        StubGateway.shared.script("GET /api/v1/projects", .json(#"{"error":"not_found"}"#, status: 404))
        _ = try? await RemoteStub.client().projects()
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 2)
    }

    func testALongRetryAfterIsLeftToTheScreensBackoff() async throws {
        RemoteStub.scriptSignIn()
        StubGateway.shared.script(
            "GET /api/v1/projects", .json(#"{"error":"busy"}"#, status: 503, headers: ["Retry-After": "30"]))
        do {
            _ = try await RemoteStub.client().projects()
            XCTFail("expected busy")
        } catch {
            XCTAssertEqual(error as? RemoteError, .busy)
        }
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 1)
    }

    func testALinkBlipIsTriedAgainButATimeoutIsNot() async throws {
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v1/projects", .fail(.networkConnectionLost), .json(RemoteStub.projectsBody(["p1"])))
        _ = try await RemoteStub.client().projects()
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 2)
        StubGateway.shared.reset()
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v1/projects", .fail(.timedOut))
        do {
            _ = try await RemoteStub.client().projects()
            XCTFail("expected a timeout")
        } catch {
            XCTAssertEqual(error as? RemoteError, .unreachable("timed out"))
        }
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 1)
    }

    func testACancelledReadIsCancelledNotUnreachable() async throws {
        RemoteStub.scriptSignIn()
        StubGateway.shared.script("GET /api/v1/projects", RemoteStub.busy)
        // Real waits: the cancel lands during the backoff.
        let client = RemoteStub.client(retry: ReadRetry(jitter: { 1 }))
        let read = Task { try await client.projects() }
        try await Task.sleep(for: .milliseconds(200))
        read.cancel()
        do {
            _ = try await read.value
            XCTFail("expected cancellation")
        } catch {
            XCTAssertEqual(error as? RemoteError, .cancelled)
        }
        XCTAssertEqual(RemoteStub.count("/api/v1/projects"), 1)
    }

    func testRetryClassesPerFailure() {
        XCTAssertEqual(RemoteError.busy.readRetry, .always)
        XCTAssertEqual(RemoteError.daemonUnavailable.readRetry, .always)
        XCTAssertEqual(RemoteError.server(status: 504, code: "").readRetry, .always)
        XCTAssertEqual(RemoteError.forbidden.readRetry, .once)
        XCTAssertEqual(RemoteError.server(status: 500, code: "").readRetry, .never)
        XCTAssertEqual(RemoteError.server(status: 404, code: "").readRetry, .never)
        XCTAssertEqual(RemoteError.unauthorized.readRetry, .never)
        XCTAssertEqual(RemoteError.cancelled.readRetry, .never)
        XCTAssertEqual(transientTransport(.networkConnectionLost), .always)
        XCTAssertEqual(transientTransport(.notConnectedToInternet), .always)
        XCTAssertEqual(transientTransport(.timedOut), .never)
        XCTAssertTrue(isOperatorRoute("/api/v2/dashboard"))
        XCTAssertTrue(isOperatorRoute("/api/v1/projects/p/sessions/s/decision-targets"))
        XCTAssertTrue(isOperatorRoute("/api/v1/projects/p/sessions/s/decisions/answer-pending"))
        XCTAssertFalse(isOperatorRoute("/api/v1/projects/p/sessions/s/decisions"))
        XCTAssertFalse(isOperatorRoute("/auth/bootstrap"))
    }
}
