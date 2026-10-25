import Foundation
import XCTest

@testable import RSIRemote

// Gateway API v2 (docs/rsi-remote/contracts-v2.md): every fixture under
// remote/contracts/v2/fixtures decodes or maps, and replayed through the
// URLProtocol stub the client sends exactly the fixture's request.

/// One fixture exchange: `{name, request, response}`.
struct V2Fixture {
    let name: String
    let method: String
    /// The path without the query.
    let path: String
    let query: String?
    let headers: [String: String]
    let requestBody: [String: Any]?
    let status: Int
    let retryAfter: Int?
    /// The response body bytes, nil for a bare status.
    let body: Data?

    static let host = "hub.example.ts.net"

    static func all() throws -> [V2Fixture] {
        let bundle = Bundle(for: V2FixtureTests.self)
        let folder = try XCTUnwrap(bundle.url(forResource: "fixtures", withExtension: nil))
        let files = try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "json" }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
        return try files.map { try V2Fixture(url: $0) }
    }

    static func named(_ name: String) throws -> V2Fixture {
        try XCTUnwrap(all().first { $0.name == name }, "fixture \(name)")
    }

    init(url: URL) throws {
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        name = try XCTUnwrap(object["name"] as? String)
        XCTAssertEqual(name + ".json", url.lastPathComponent)
        let request = try XCTUnwrap(object["request"] as? [String: Any])
        let response = try XCTUnwrap(object["response"] as? [String: Any])
        method = try XCTUnwrap(request["method"] as? String)
        let full = try XCTUnwrap(request["path"] as? String)
        if let mark = full.firstIndex(of: "?") {
            path = String(full[..<mark])
            query = String(full[full.index(after: mark)...])
        } else {
            path = full
            query = nil
        }
        let symbolic = try XCTUnwrap(request["headers"] as? [String: String])
        headers = symbolic.mapValues {
            $0.replacingOccurrences(of: "<canonical_host>", with: Self.host)
        }
        requestBody = request["body"] as? [String: Any]
        status = try XCTUnwrap(response["status"] as? Int)
        retryAfter = response["retry_after"] as? Int
        if let value = response["body"], !(value is NSNull) {
            body = try JSONSerialization.data(withJSONObject: value)
        } else {
            body = nil
        }
    }

    /// The stub reply: the fixture's status, body and `Retry-After`.
    var reply: StubGateway.Reply {
        var headers: [String: String] = [:]
        if let retryAfter { headers["Retry-After"] = "\(retryAfter)" }
        if body != nil {
            headers["Content-Type"] = "application/json"
        } else {
            headers["Content-Type"] = "text/plain"
        }
        let bare = Data("\(status)".utf8)
        return StubGateway.Reply(status: status, headers: headers, body: body ?? bare)
    }

    /// `/api/v2/projects/{project}/managers/{seat}/…` → (project, seat).
    var projectAndSeat: (String, String)? {
        let parts = path.split(separator: "/").map(String.init)
        guard parts.count >= 6, parts[2] == "projects", parts[4] == "managers" else { return nil }
        return (parts[3], parts[5])
    }

    func decode<T: Decodable>(_ type: T.Type) throws -> T {
        try JSONDecoder().decode(type, from: XCTUnwrap(body))
    }
}

private let project = "550e8400-e29b-41d4-a716-446655440000"
private let pmSeat = "550e8400-e29b-41d4-a716-446655440001"
private let globalSeat = "550e8400-e29b-41d4-a716-446655440002"
private let workerSeat = "550e8400-e29b-41d4-a716-446655440003"
private let fixtureKey = "550e8400-e29b-41d4-a716-446655440009"
private let messageId = "550e8400-e29b-41d4-a716-44665544000a"
private let fixtureText = "please prioritize the migration, tell nobody secret-token-text"

private func v2Client(retry: ReadRetry = .instant) -> RemoteClient {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [StubProtocol.self]
    return RemoteClient(host: V2Fixture.host, session: URLSession(configuration: configuration), retry: retry)
}

private func v2SignIn() {
    StubGateway.shared.script("GET /auth/bootstrap", .json(#"{"nonce":"n-1"}"#))
    StubGateway.shared.script("POST /auth/session", .json(#"{"csrf":"csrf-1"}"#), .json(#"{"csrf":"csrf-2"}"#))
}

private func jsonObject(_ data: Data) throws -> NSDictionary {
    try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? NSDictionary)
}

// MARK: Decoding

final class V2FixtureTests: XCTestCase {
    /// The fixture list of the contract's "Fixtures" section.
    static let contractNames: Set<String> = [
        "projects_with_global_200", "projects_without_global_200", "projects_empty_200",
        "conversation_first_page_200", "conversation_older_page_200", "conversation_empty_200",
        "conversation_unknown_seat_404", "unauthenticated_401", "message_accepted_200",
        "message_replayed_200", "message_switch_off_403", "message_bad_csrf_403",
        "message_not_a_seat_403", "message_invalid_400", "message_key_conflict_409",
        "message_rate_limited_429", "message_outcome_unknown_504", "message_busy_503",
        "dashboard_200", "dashboard_empty_200", "dashboard_incomplete_200", "dashboard_busy_503",
        "providers_200", "providers_empty_200", "providers_busy_503",
    ]

    /// What each error fixture means to the app.
    static let messageErrors: [String: (MessageErrorKind, Int?)] = [
        "message_switch_off_403": (.forbidden, nil),
        "message_bad_csrf_403": (.forbidden, nil),
        "message_not_a_seat_403": (.forbidden, nil),
        "message_invalid_400": (.invalid, nil),
        "message_key_conflict_409": (.conflict, nil),
        "message_rate_limited_429": (.rateLimited, 1),
        "message_outcome_unknown_504": (.unknown, nil),
        "message_busy_503": (.busy, 1),
    ]

    func testTheFixtureFolderIsExactlyTheContractList() throws {
        XCTAssertEqual(Set(try V2Fixture.all().map(\.name)), Self.contractNames)
    }

    func testEveryFixtureDecodesOrMaps() throws {
        for fixture in try V2Fixture.all() {
            let label = fixture.name
            switch (fixture.method, fixture.status) {
            case ("GET", 200) where fixture.path == "/api/v2/projects":
                XCTAssertNoThrow(try fixture.decode(ProjectSeatsList.self), label)
            case ("GET", 200) where fixture.path == "/api/v2/dashboard":
                XCTAssertNoThrow(try fixture.decode(Dashboard.self), label)
            case ("GET", 200) where fixture.path == "/api/v2/providers":
                XCTAssertNoThrow(try fixture.decode(ProviderUsage.self), label)
            case ("GET", 503):
                XCTAssertTrue(["/api/v2/dashboard", "/api/v2/providers"].contains(fixture.path), label)
                XCTAssertEqual(fixture.retryAfter, 2, label)
                XCTAssertEqual(
                    statusError(503, code: errorCode(from: try XCTUnwrap(fixture.body))), .busy, label)
            case ("GET", 200):
                XCTAssertTrue(fixture.path.hasSuffix("/conversation"), label)
                XCTAssertNoThrow(try fixture.decode(ConversationPage.self), label)
            case ("POST", 200):
                XCTAssertNoThrow(try fixture.decode(MessageReceipt.self), label)
            case ("POST", let status):
                let (kind, retry) = try XCTUnwrap(Self.messageErrors[label], label)
                let code = fixture.body.map(errorCode(from:)) ?? ""
                let mapped = messageRefusal(status: status, code: code, retryAfter: fixture.retryAfter)
                XCTAssertEqual(mapped.kind, kind, label)
                XCTAssertEqual(mapped.retryAfter, retry, label)
                XCTAssertEqual(mapped.status, status, label)
            case ("GET", 404):
                XCTAssertEqual(
                    statusError(404, code: errorCode(from: try XCTUnwrap(fixture.body))),
                    .server(status: 404, code: "not_found"), label)
            case ("GET", 401):
                XCTAssertNil(fixture.body, label)
                XCTAssertEqual(statusError(401, code: ""), .unauthorized, label)
            default:
                XCTFail("unmapped fixture \(label)")
            }
        }
    }

    func testProjectsCarrySeatsAndThePendingBadge() throws {
        let with = try V2Fixture.named("projects_with_global_200").decode(ProjectSeatsList.self)
        let row = try XCTUnwrap(with.projects.first)
        XCTAssertEqual(row.projectId, project)
        XCTAssertEqual(row.name, "Alpha")
        let pm = try XCTUnwrap(row.projectManager)
        XCTAssertEqual(pm.sessionId, pmSeat)
        XCTAssertEqual(pm.role, SeatRole.projectManager.rawValue)
        XCTAssertEqual(pm.pendingBadge, "2")
        XCTAssertEqual(pm.summary, "Running · Claude claude-opus-5-5")
        let global = try XCTUnwrap(row.globalManager)
        XCTAssertEqual(global.role, SeatRole.globalManager.rawValue)
        XCTAssertNil(global.pendingDecisions)
        XCTAssertNil(global.pendingBadge)
        XCTAssertEqual(row.seat(.globalManager)?.sessionId, globalSeat)
        XCTAssertTrue(row.lists(pmSeat))
        XCTAssertFalse(row.lists(workerSeat))

        let without = try V2Fixture.named("projects_without_global_200").decode(ProjectSeatsList.self)
        XCTAssertNil(try XCTUnwrap(without.projects.first).globalManager)
        XCTAssertEqual(
            try V2Fixture.named("projects_empty_200").decode(ProjectSeatsList.self).projects, [])
    }

    func testConversationPagesWalkBackWithTheCursor() throws {
        let first = try V2Fixture.named("conversation_first_page_200").decode(ConversationPage.self)
        XCTAssertEqual(first.items.map(\.id), ["904", "903"])
        XCTAssertEqual(first.items.map(\.speakerLabel), ["Manager", "You"])
        XCTAssertEqual(first.items[1].operatorMessageId, messageId)
        XCTAssertEqual(first.nextBefore, "3:903")
        XCTAssertTrue(first.hasMore)
        XCTAssertEqual(first.operatorMessages.first?.state, "delivered")
        XCTAssertEqual(first.operatorMessages.first?.textLen, 32)
        XCTAssertEqual(first.seat.sessionId, pmSeat)

        let older = try V2Fixture.named("conversation_older_page_200")
        XCTAssertEqual(older.query, "before=3:903")
        let page = try older.decode(ConversationPage.self)
        XCTAssertEqual(page.items.first?.speakerLabel, "System")
        XCTAssertNil(page.nextBefore)
        XCTAssertFalse(page.hasMore)
        XCTAssertTrue(page.operatorMessages.isEmpty)

        let empty = try V2Fixture.named("conversation_empty_200").decode(ConversationPage.self)
        XCTAssertTrue(empty.items.isEmpty)
        XCTAssertNil(empty.nextBefore)
    }

    func testReceiptsShowAcceptedAndReplayed() throws {
        let accepted = try V2Fixture.named("message_accepted_200").decode(MessageReceipt.self)
        XCTAssertEqual(accepted.messageId, messageId)
        XCTAssertEqual(accepted.seatSessionId, pmSeat)
        XCTAssertEqual(accepted.state, "queued")
        XCTAssertFalse(accepted.deduplicated)
        let replayed = try V2Fixture.named("message_replayed_200").decode(MessageReceipt.self)
        XCTAssertTrue(replayed.deduplicated)
        XCTAssertEqual(replayed.messageId, accepted.messageId)
    }

    func testTheRequestBodyIsExactlyTheContractShape() throws {
        let fixture = try V2Fixture.named("message_accepted_200")
        let request = MessageRequest(project: project, seat: pmSeat, text: fixtureText, idempotencyKey: fixtureKey)
        XCTAssertEqual(request.path, fixture.path)
        XCTAssertEqual(try jsonObject(request.body()), try XCTUnwrap(fixture.requestBody) as NSDictionary)
        XCTAssertEqual(
            String(decoding: try request.body(), as: UTF8.self),
            #"{"idempotency_key":"550e8400-e29b-41d4-a716-446655440009","text":"please prioritize the migration, tell nobody secret-token-text"}"#)
    }

    func testMessageTextRulesMatchTheGateway() {
        XCTAssertNil(messageProblem("ship it"))
        XCTAssertNil(messageProblem(String(repeating: "x", count: 4096)))
        XCTAssertNotNil(messageProblem(""))
        XCTAssertNotNil(messageProblem("   \n"))
        XCTAssertNotNil(messageProblem("a\0b"))
        XCTAssertNotNil(messageProblem(String(repeating: "x", count: 4097)))
        XCTAssertNotNil(messageProblem(String(repeating: "é", count: 2049)))  // 4098 bytes
        for bad in [
            MessageRequest(project: project, seat: pmSeat, text: "", idempotencyKey: fixtureKey),
            MessageRequest(project: project, seat: pmSeat, text: "a", idempotencyKey: "NOT-A-UUID"),
            MessageRequest(
                project: project, seat: pmSeat, text: "a",
                idempotencyKey: "550E8400-E29B-41D4-A716-446655440009"),
        ] {
            XCTAssertThrowsError(try bad.body())
        }
    }

    func testTheCursorGrammarIsTheGatewaysBeforeGrammar() {
        for good in ["3:903", "0:1", "-1:5", "2147483647:9223372036854775807"] {
            XCTAssertTrue(isConversationCursor(good), good)
        }
        for bad in ["", "3", "3:", ":903", "3:0", "3:-1", "3:0903", "a:1", "3:9x", "1:2:3", "2147483648:1", "٣:903"] {
            XCTAssertFalse(isConversationCursor(bad), bad)
        }
    }

    func testRefusalMappingCoversEveryContractStatus() {
        XCTAssertEqual(messageRefusal(status: 409, code: "seat_not_live", retryAfter: nil).kind, .seatChanged)
        XCTAssertEqual(messageRefusal(status: 404, code: "", retryAfter: nil).kind, .seatChanged)
        XCTAssertEqual(messageRefusal(status: 503, code: "audit_unavailable", retryAfter: 1).kind, .busy)
        XCTAssertEqual(messageRefusal(status: 502, code: "outcome_unknown", retryAfter: nil).kind, .unknown)
        XCTAssertEqual(messageRefusal(status: 502, code: "", retryAfter: nil).kind, .unavailable)
        XCTAssertEqual(messageRefusal(status: 504, code: "", retryAfter: nil).kind, .unavailable)
        XCTAssertEqual(messageRefusal(status: 401, code: "", retryAfter: nil).kind, .session)
        XCTAssertEqual(messageRefusal(status: 500, code: "", retryAfter: nil).kind, .unexpected)
        let unknown = messageRefusal(status: 504, code: "outcome_unknown", retryAfter: nil)
        XCTAssertTrue(unknown.message.contains("Not known whether delivered"))
        XCTAssertTrue(unknown.mayHaveSent)
        let limited = messageRefusal(status: 429, code: "rate_limited", retryAfter: 7)
        XCTAssertTrue(limited.message.contains("7 seconds"))
        XCTAssertFalse(limited.mayHaveSent)
        XCTAssertTrue(MessageError.messagingOff().message.contains("Messaging from the phone is off"))
    }

    func testDeliveryStatesReadPlainly() {
        XCTAssertEqual(describeDelivery("queued").tone, .busy)
        XCTAssertEqual(describeDelivery("dispatching").tone, .busy)
        XCTAssertEqual(describeDelivery("effect_possible").tone, .busy)
        XCTAssertEqual(describeDelivery("delivered").tone, .ok)
        XCTAssertTrue(describeDelivery("uncertain").text.contains("will not be sent again"))
        XCTAssertEqual(describeDelivery("withdrawn").tone, .warn)
        XCTAssertEqual(describeDelivery("future_state").text, "State: future_state.")
    }

    func testSettingsNamesBothContracts() {
        XCTAssertEqual(supportedContracts, ["v1", "v2"])
        XCTAssertEqual(AppInfo.contract, "RSI Remote v1 + v2")
    }

    func testTheProjectBadge() throws {
        let seats = try XCTUnwrap(
            V2Fixture.named("projects_with_global_200").decode(ProjectSeatsList.self).projects.first)
        var row = ProjectRow(project: WireProject(id: project, name: "Alpha"), waiting: 3, sessions: [])
        XCTAssertEqual(row.badge, "3")
        row.seats = seats
        XCTAssertEqual(row.badge, "2")
    }
}

// MARK: Poll backoff

final class V2ScheduleTests: XCTestCase {
    func testPollBackoffDoublesToTwoMinutesAndResetsOnSuccess() {
        var backoff = PollBackoff()
        var schedule = [backoff.interval]
        for _ in 0..<6 {
            backoff.failed()
            schedule.append(backoff.interval)
        }
        XCTAssertEqual(schedule, [10, 20, 40, 80, 120, 120, 120].map { Duration.seconds($0) })
        backoff.succeeded()
        XCTAssertEqual(backoff.interval, .seconds(10))
        backoff.failed(retryAfter: 300)
        XCTAssertEqual(backoff.interval, .seconds(300))
        backoff.failed(retryAfter: 1)
        XCTAssertEqual(backoff.interval, .seconds(40))
        for _ in 0..<100 { backoff.failed() }
        XCTAssertEqual(backoff.interval, .seconds(120))
    }
}

// MARK: Client against the stub gateway

final class V2ClientTests: XCTestCase {
    override func setUp() {
        super.setUp()
        StubGateway.shared.reset()
    }

    /// Replay one fixture: script its response, make the matching call and
    /// check the request the client sent against the fixture's request.
    private func replay(_ fixture: V2Fixture) async throws -> Result<Any, any Error> {
        StubGateway.shared.reset()
        v2SignIn()
        StubGateway.shared.script("\(fixture.method) \(fixture.path)", fixture.reply)
        let client = v2Client()
        let result: Result<Any, any Error>
        do {
            if fixture.path == "/api/v2/projects" {
                result = .success(try await client.managerSeats())
            } else if fixture.path == "/api/v2/dashboard" {
                result = .success(try await client.dashboard())
            } else if fixture.path == "/api/v2/providers" {
                result = .success(try await client.providers())
            } else if fixture.method == "GET" {
                let (project, seat) = try XCTUnwrap(fixture.projectAndSeat)
                let before = fixture.query.map { String($0.dropFirst("before=".count)) }
                result = .success(try await client.conversation(project: project, seat: seat, before: before))
            } else {
                let (project, seat) = try XCTUnwrap(fixture.projectAndSeat)
                let body = try XCTUnwrap(fixture.requestBody)
                let request = MessageRequest(
                    project: project, seat: seat, text: try XCTUnwrap(body["text"] as? String),
                    idempotencyKey: try XCTUnwrap(body["idempotency_key"] as? String))
                result = .success(try await client.sendMessage(request))
            }
        } catch {
            result = .failure(error)
        }
        return result
    }

    private func sent(_ fixture: V2Fixture) -> [StubGateway.Seen] {
        StubGateway.shared.requests.filter { $0.method == fixture.method && $0.path == fixture.path }
    }

    func testEveryFixtureRequestIsWhatTheClientSends() async throws {
        for fixture in try V2Fixture.all() {
            let label = fixture.name
            let result = try await replay(fixture)
            let requests = sent(fixture)
            if fixture.name == "message_invalid_400" {
                // Empty text is refused before anything is sent.
                XCTAssertTrue(requests.isEmpty, label)
                guard case .failure(let error as MessageError) = result else {
                    XCTFail("\(label): expected a local refusal"); continue
                }
                XCTAssertEqual(error.kind, .invalid, label)
                continue
            }
            // A 401 re-bootstraps once and resends; a busy read is tried again
            // up to the read limit (#350); nothing else is retried, no write ever.
            let expected =
                switch (fixture.method, fixture.status) {
                case (_, 401): 2
                case ("GET", 503): ReadRetry.standard.attempts
                default: 1
                }
            XCTAssertEqual(requests.count, expected, label)
            guard let seen = requests.first else { continue }
            XCTAssertEqual(seen.query, fixture.query, label)
            XCTAssertEqual(seen.headers["origin"], fixture.headers["origin"], label)
            XCTAssertEqual(seen.headers["origin"], "https://\(V2Fixture.host)", label)
            if fixture.method == "POST" {
                XCTAssertEqual(seen.headers["content-type"], fixture.headers["content-type"], label)
                XCTAssertNotNil(fixture.headers["x-rsi-csrf"], label)
                XCTAssertEqual(seen.headers["x-rsi-csrf"], "csrf-1", label)
                XCTAssertEqual(try jsonObject(seen.body), try XCTUnwrap(fixture.requestBody) as NSDictionary, label)
            } else {
                XCTAssertNil(seen.headers["x-rsi-csrf"], label)
                XCTAssertTrue(seen.body.isEmpty, label)
            }
            switch (fixture.status, result) {
            case (200, .success):
                break
            case (401, .failure(let error)):
                XCTAssertEqual(error as? RemoteError, .unauthorized, label)
            case (404, .failure(let error)):
                XCTAssertEqual((error as? ReadFailure)?.error, .server(status: 404, code: "not_found"), label)
            case (503, .failure(let error)) where fixture.method == "GET":
                XCTAssertEqual(error as? RemoteError, .busy, label)
            case (let status, .failure(let error)) where fixture.method == "POST":
                let (kind, retry) = try XCTUnwrap(V2FixtureTests.messageErrors[label], label)
                let refusal = try XCTUnwrap(error as? MessageError, label)
                XCTAssertEqual(refusal.kind, kind, label)
                XCTAssertEqual(refusal.status, status, label)
                XCTAssertEqual(refusal.retryAfter, retry, label)
            default:
                XCTFail("\(label): unexpected result \(result)")
            }
        }
    }

    func testA401OnSendReBootstrapsOnceAndResendsTheIdenticalBody() async throws {
        let accepted = try V2Fixture.named("message_accepted_200")
        v2SignIn()
        StubGateway.shared.script(
            "POST \(accepted.path)", .json("unauthorized", status: 401), accepted.reply)
        let request = MessageRequest(project: project, seat: pmSeat, text: fixtureText, idempotencyKey: fixtureKey)
        let receipt = try await v2Client().sendMessage(request)
        XCTAssertEqual(receipt.messageId, messageId)
        let posts = sent(accepted)
        XCTAssertEqual(posts.count, 2)
        XCTAssertEqual(posts[0].body, posts[1].body)
        XCTAssertEqual(try jsonObject(posts[1].body)["idempotency_key"] as? String, fixtureKey)
        XCTAssertEqual(posts.map { $0.headers["x-rsi-csrf"] }, ["csrf-1", "csrf-2"])
        XCTAssertEqual(StubGateway.shared.requests.filter { $0.path == "/auth/session" }.count, 2)
    }

    func testASecond401OnSendIsASessionErrorWithNoThirdAttempt() async throws {
        let accepted = try V2Fixture.named("message_accepted_200")
        v2SignIn()
        StubGateway.shared.script("POST \(accepted.path)", .json("unauthorized", status: 401))
        let request = MessageRequest(project: project, seat: pmSeat, text: fixtureText, idempotencyKey: fixtureKey)
        do throws(MessageError) {
            _ = try await v2Client().sendMessage(request)
            XCTFail("expected a refusal")
        } catch {
            XCTAssertEqual(error.kind, .session)
        }
        XCTAssertEqual(sent(accepted).count, 2)
    }

    func testALostConnectionIsUnknownAndAnUnreachableHostSentNothing() async throws {
        let accepted = try V2Fixture.named("message_accepted_200")
        let request = MessageRequest(project: project, seat: pmSeat, text: fixtureText, idempotencyKey: fixtureKey)
        for (failure, kind) in [(URLError.Code.networkConnectionLost, MessageErrorKind.unknown), (.timedOut, .unknown), (.cannotConnectToHost, .unavailable)] {
            StubGateway.shared.reset()
            v2SignIn()
            StubGateway.shared.script("POST \(accepted.path)", .fail(failure))
            do throws(MessageError) {
                _ = try await v2Client().sendMessage(request)
                XCTFail("expected \(kind)")
            } catch {
                XCTAssertEqual(error.kind, kind, "\(failure)")
            }
        }
    }

    func testAnInvalidCursorIsNeverSent() async {
        v2SignIn()
        do throws(ReadFailure) {
            _ = try await v2Client().conversation(project: project, seat: pmSeat, before: "3:903&x=1")
            XCTFail("expected a refusal")
        } catch {
            XCTAssertEqual(error.error, .invalidResponse("conversation cursor"))
        }
        XCTAssertTrue(StubGateway.shared.requests.filter { $0.path.hasSuffix("/conversation") }.isEmpty)
    }
}

// MARK: Manager chat

@MainActor
final class ManagerChatTests: XCTestCase {
    override func setUp() {
        super.setUp()
        StubGateway.shared.reset()
        v2SignIn()
    }

    private var conversationPath: String { managerPath(project, pmSeat) + "/conversation" }

    private func seats(pm: String = pmSeat) throws -> ProjectSeats {
        let row = try XCTUnwrap(
            V2Fixture.named("projects_with_global_200").decode(ProjectSeatsList.self).projects.first)
        guard pm != pmSeat, let seat = row.projectManager else { return row }
        let moved = ManagerSeat(
            sessionId: pm, role: seat.role, status: seat.status, provider: seat.provider, model: seat.model,
            lastActivityAt: seat.lastActivityAt, pendingDecisions: seat.pendingDecisions,
            pendingDecisionsTruncated: seat.pendingDecisionsTruncated)
        return ProjectSeats(
            projectId: row.projectId, name: row.name, projectManager: moved, globalManager: row.globalManager)
    }

    private func makeChat(pm: String = pmSeat, retry: ReadRetry = .instant) throws -> ManagerChat {
        ManagerChat(project: project, seats: try seats(pm: pm), client: v2Client(retry: retry))
    }

    func testTheTranscriptLoadsOlderPagesAndPollsWithoutDuplicates() async throws {
        StubGateway.shared.script("GET \(conversationPath)", try V2Fixture.named("conversation_first_page_200").reply)
        let chat = try makeChat()
        await chat.refresh()
        XCTAssertEqual(chat.thread?.items.map(\.id), ["904", "903"])
        XCTAssertEqual(chat.thread?.hasMore, true)
        XCTAssertEqual(chat.backoff.failures, 0)

        StubGateway.shared.script(
            "GET \(conversationPath)", try V2Fixture.named("conversation_older_page_200").reply)
        await chat.loadOlder()
        XCTAssertEqual(StubGateway.shared.requests.last?.query, "before=3:903")
        XCTAssertEqual(chat.thread?.items.map(\.id), ["904", "903", "902"])
        XCTAssertEqual(chat.thread?.hasMore, false)
        XCTAssertNil(chat.thread?.olderCursor)

        // The next poll returns the first page again: no duplicates, older pages kept.
        StubGateway.shared.script("GET \(conversationPath)", try V2Fixture.named("conversation_first_page_200").reply)
        await chat.refresh()
        XCTAssertEqual(chat.thread?.items.map(\.id), ["904", "903", "902"])
        XCTAssertEqual(chat.thread?.hasMore, false)
    }

    func testReadErrorsBackOffAndASuccessResets() async throws {
        StubGateway.shared.script(
            "GET \(conversationPath)", .json(#"{"error":"busy"}"#, status: 503, headers: ["Retry-After": "1"]),
            .json(#"{"error":"busy"}"#, status: 503), .fail(.timedOut),
            try V2Fixture.named("conversation_empty_200").reply)
        // One attempt per read, so each poll sees one failure.
        let chat = try makeChat(retry: ReadRetry(attempts: 1))
        var schedule: [Duration] = []
        for _ in 0..<4 {
            await chat.refresh()
            schedule.append(chat.backoff.interval)
        }
        XCTAssertEqual(schedule, [20, 40, 80, 10].map { Duration.seconds($0) })
        XCTAssertEqual(chat.thread?.loaded, true)
        XCTAssertNil(chat.thread?.error)
    }

    func testAPollRidesOutBusyAnswersInsideOneRead() async throws {
        StubGateway.shared.script(
            "GET \(conversationPath)", .json(#"{"error":"busy"}"#, status: 503, headers: ["Retry-After": "2"]),
            .json(#"{"error":"busy"}"#, status: 503), try V2Fixture.named("conversation_first_page_200").reply)
        let chat = try makeChat()
        await chat.refresh()
        XCTAssertEqual(chat.thread?.items.map(\.id), ["904", "903"])
        XCTAssertNil(chat.thread?.error)
        XCTAssertEqual(chat.backoff.failures, 0)
        XCTAssertEqual(StubGateway.shared.requests.filter { $0.path == conversationPath }.count, 3)
    }

    func testACancelledPollLeavesTheThreadAsItWas() async throws {
        StubGateway.shared.script("GET \(conversationPath)", try V2Fixture.named("conversation_first_page_200").reply)
        let chat = try makeChat()
        await chat.refresh()
        StubGateway.shared.script(
            "GET \(conversationPath)", try V2Fixture.named("conversation_first_page_200").reply.after(5))
        let poll = Task { await chat.refresh() }
        try await Task.sleep(for: .milliseconds(100))
        poll.cancel()
        await poll.value
        XCTAssertNil(chat.thread?.error)
        XCTAssertEqual(chat.backoff.failures, 0)
        XCTAssertEqual(chat.thread?.items.map(\.id), ["904", "903"])
    }

    func testAStaleSeatConversationReListsAndReadsTheNewSeat() async throws {
        let stale = managerPath(project, workerSeat) + "/conversation"
        StubGateway.shared.script("GET \(stale)", try V2Fixture.named("conversation_unknown_seat_404").reply)
        StubGateway.shared.script("GET /api/v2/projects", try V2Fixture.named("projects_with_global_200").reply)
        StubGateway.shared.script("GET \(conversationPath)", try V2Fixture.named("conversation_first_page_200").reply)
        let chat = try makeChat(pm: workerSeat)
        await chat.refresh()
        XCTAssertEqual(chat.seat?.sessionId, pmSeat)
        XCTAssertEqual(chat.thread?.items.map(\.id), ["904", "903"])
    }

    func testTheGlobalToggleReadsTheGlobalSeat() async throws {
        let globalPath = managerPath(project, globalSeat) + "/conversation"
        StubGateway.shared.script("GET \(globalPath)", try V2Fixture.named("conversation_empty_200").reply)
        let chat = try makeChat()
        chat.role = .globalManager
        await chat.refresh()
        XCTAssertEqual(StubGateway.shared.requests.last?.path, globalPath)
        XCTAssertEqual(chat.thread?.seat, globalSeat)
        XCTAssertNil(chat.threads[.projectManager])
    }
}

/// hub #2988: `GET /api/v2/projects` reports `messages_enabled`, so the phone
/// closes the compose box before a send fails with a bare 403.
@MainActor
final class MessagesEnabledTests: XCTestCase {
    private func list(_ json: String) throws -> ProjectSeatsList {
        try JSONDecoder().decode(ProjectSeatsList.self, from: Data(json.utf8))
    }

    func testTheFlagDecodesAndIsNilFromAnOlderGateway() throws {
        XCTAssertEqual(try list(#"{"projects":[],"messages_enabled":true}"#).messagesEnabled, true)
        XCTAssertEqual(try list(#"{"projects":[],"messages_enabled":false}"#).messagesEnabled, false)
        XCTAssertNil(try list(#"{"projects":[]}"#).messagesEnabled)
    }

    func testTheChatClosesTheComposerWhileMessagingIsOff() {
        let chat = ManagerChat(project: project, seats: nil, client: RemoteClient(host: V2Fixture.host))
        XCTAssertFalse(chat.composeDisabled)
        XCTAssertFalse(chat.messagingOff)
        chat.update(messagesEnabled: false)
        XCTAssertTrue(chat.composeDisabled)
        XCTAssertTrue(chat.messagingOff)
        // An older gateway's list (no flag) keeps what the chat knows.
        chat.update(messagesEnabled: nil)
        XCTAssertTrue(chat.composeDisabled)
        chat.update(messagesEnabled: true)
        XCTAssertFalse(chat.composeDisabled)
        XCTAssertFalse(chat.messagingOff)
    }

    func testTheAppModelHandsTheFlagToEveryChat() throws {
        let suite = "rsi-remote-tests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let model = AppModel(defaults: defaults)
        XCTAssertTrue(model.setHost(V2Fixture.host))
        let early = try XCTUnwrap(model.chat(project: project))
        model.applySeats(ProjectSeatsList(projects: [], messagesEnabled: false))
        XCTAssertTrue(early.composeDisabled)
        // A chat opened later starts from the last flag the gateway reported.
        let later = try XCTUnwrap(model.chat(project: "00000000-0000-4000-8000-0000000000ff"))
        XCTAssertTrue(later.composeDisabled)
        model.applySeats(ProjectSeatsList(projects: [], messagesEnabled: true))
        XCTAssertFalse(early.composeDisabled)
        XCTAssertFalse(later.composeDisabled)
    }
}
