import Foundation
import XCTest

@testable import RSIRemote

// MARK: URLProtocol stub

/// A canned gateway: records each request and answers from a script.
final class StubGateway: @unchecked Sendable {
    struct Seen: Sendable {
        let method: String
        let path: String
        let headers: [String: String]
        let body: Data
        var query: String? = nil
    }

    struct Reply: Sendable {
        var status = 200
        var headers: [String: String] = [:]
        var body = Data()
        var failure: URLError.Code?
        /// Seconds before the reply arrives (the request stays in flight).
        var delay: Double = 0

        static func json(_ text: String, status: Int = 200, headers: [String: String] = [:]) -> Reply {
            Reply(status: status, headers: headers, body: Data(text.utf8))
        }
        static func fail(_ code: URLError.Code) -> Reply { Reply(failure: code) }

        /// The same reply, `seconds` later.
        func after(_ seconds: Double) -> Reply {
            var reply = self
            reply.delay = seconds
            return reply
        }
    }

    static let shared = StubGateway()

    private let lock = NSLock()
    private var seenRequests: [Seen] = []
    private var queued: [String: [Reply]] = [:]
    private var inFlight = 0
    private var operatorInFlight = 0
    private var peak = 0
    private var operatorPeak = 0
    /// Bumped by `reset`, so a late reply to an earlier test is not counted.
    private(set) var epoch = 0

    func reset() {
        lock.lock(); defer { lock.unlock() }
        epoch += 1
        seenRequests = []
        queued = [:]
        inFlight = 0
        operatorInFlight = 0
        peak = 0
        operatorPeak = 0
    }

    /// The most requests in flight at once since the reset: all, and on the
    /// gateway's operator routes.
    var peaks: (all: Int, operator: Int) {
        lock.lock(); defer { lock.unlock() }
        return (peak, operatorPeak)
    }

    func finished(_ seen: Seen, epoch: Int) {
        lock.lock(); defer { lock.unlock() }
        guard epoch == self.epoch else { return }
        inFlight -= 1
        if isOperatorRoute(seen.path) { operatorInFlight -= 1 }
    }

    /// Queue replies for `METHOD path`; the last one repeats.
    func script(_ route: String, _ replies: Reply...) {
        lock.lock(); defer { lock.unlock() }
        queued[route] = replies
    }

    var requests: [Seen] {
        lock.lock(); defer { lock.unlock() }
        return seenRequests
    }

    func respond(_ seen: Seen) -> (Reply, epoch: Int) {
        lock.lock(); defer { lock.unlock() }
        seenRequests.append(seen)
        inFlight += 1
        peak = max(peak, inFlight)
        if isOperatorRoute(seen.path) {
            operatorInFlight += 1
            operatorPeak = max(operatorPeak, operatorInFlight)
        }
        let route = "\(seen.method) \(seen.path)"
        guard var replies = queued[route], let first = replies.first else {
            return (Reply(status: 404), epoch)
        }
        if replies.count > 1 { replies.removeFirst(); queued[route] = replies }
        return (first, epoch)
    }
}

final class StubProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        var body = request.httpBody ?? Data()
        if let stream = request.httpBodyStream {
            stream.open()
            defer { stream.close() }
            var buffer = [UInt8](repeating: 0, count: 4096)
            while stream.hasBytesAvailable {
                let count = stream.read(&buffer, maxLength: buffer.count)
                if count <= 0 { break }
                body.append(buffer, count: count)
            }
        }
        var headers: [String: String] = [:]
        for (name, value) in request.allHTTPHeaderFields ?? [:] { headers[name.lowercased()] = value }
        let seen = StubGateway.Seen(
            method: request.httpMethod ?? "GET", path: request.url?.path ?? "", headers: headers,
            body: body, query: request.url?.query)
        let (reply, epoch) = StubGateway.shared.respond(seen)
        guard reply.delay > 0 else {
            deliver(reply, seen, epoch)
            return
        }
        // Deliver later on this loading thread's run loop, as URLProtocol requires.
        let loop = LoadingLoop(loop: CFRunLoopGetCurrent())
        DispatchQueue.global().asyncAfter(deadline: .now() + reply.delay) {
            CFRunLoopPerformBlock(loop.loop, CFRunLoopMode.commonModes.rawValue) { self.deliver(reply, seen, epoch) }
            CFRunLoopWakeUp(loop.loop)
        }
    }

    /// The loading thread's run loop, handed to the delivery queue; CFRunLoop
    /// calls are thread-safe.
    private struct LoadingLoop: @unchecked Sendable {
        let loop: CFRunLoop
    }

    private let state = NSLock()
    private var stopped = false
    private var delivered = false

    /// Answer once, unless the task was cancelled meanwhile; either way the
    /// request leaves the in-flight count.
    private func deliver(_ reply: StubGateway.Reply, _ seen: StubGateway.Seen, _ epoch: Int) {
        state.lock()
        let first = !delivered
        let answer = first && !stopped
        delivered = true
        state.unlock()
        if answer { send(reply) }
        if first { StubGateway.shared.finished(seen, epoch: epoch) }
    }

    private func send(_ reply: StubGateway.Reply) {
        if let code = reply.failure {
            client?.urlProtocol(self, didFailWithError: URLError(code))
            return
        }
        let response = HTTPURLResponse(
            url: request.url!, statusCode: reply.status, httpVersion: "HTTP/1.1",
            headerFields: reply.headers)!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: reply.body)
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {
        state.lock()
        stopped = true
        state.unlock()
    }
}

extension ReadRetry {
    /// The standard attempts and backoff, with no waiting (tests).
    static let instant = ReadRetry(jitter: { 1 }, sleep: { _ in })
}

private let projectId = "proj-1"
private let sessionId = "550e8400-e29b-41d4-a716-446655440001"
private let basePath = "/api/v1/projects/proj-1/sessions/\(sessionId)"
private let key1 = "11111111-1111-4111-8111-111111111111"
private let key2 = "22222222-2222-4222-8222-222222222222"
private let receiptKey = "33333333-3333-4333-8333-333333333333"

private func stubbedClient() -> RemoteClient {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [StubProtocol.self]
    return RemoteClient(
        host: "hub.example.ts.net", session: URLSession(configuration: configuration), retry: .instant)
}

private func signIn(csrf: String = "csrf-1") {
    let gateway = StubGateway.shared
    gateway.script("GET /auth/bootstrap", .json(#"{"nonce":"n-1"}"#))
    gateway.script("POST /auth/session", .json(#"{"csrf":"\#(csrf)"}"#))
}

private func decisionRequest(
    answer: String = "Yes", key: String = key1
) -> AnswerRequest {
    AnswerRequest(
        decisionKey: "decision-1", pending: false, rowVersion: "7", digest: "digest-1",
        fence: AnswerFence(policyVersion: "3", scopeVersion: "4"), answer: answer,
        idempotencyKey: key)
}

private func json(_ data: Data) throws -> [String: Any] {
    try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
}

// MARK: Parsing

final class AnswerParsingTests: XCTestCase {
    private let targetsBody = """
        {"manager":"configured","fence":{"policy_version":"3","scope_version":"4"},
         "items":[{"decision_key":"decision-1","row_version":"7","target_digest":"digest-1",
                   "kind":"question","title":"Which branch?","detail":"pick one",
                   "options":["main","rolling"]}],
         "pending_items":[{"decision_id":"pending-approval:abc","target_digest":"digest-2",
                   "kind":"approval","title":"Run tool","detail":"",
                   "options":["Allow","Deny"],
                   "receipt":{"state":"queued","receipt_key":"\(receiptKey)"}}],
         "truncated":false}
        """

    func testTargetsKeepVersionsAndDigestsVerbatim() throws {
        let targets = try parseTargets(Data(targetsBody.utf8))
        XCTAssertTrue(targets.ready)
        XCTAssertEqual(targets.fence, AnswerFence(policyVersion: "3", scopeVersion: "4"))
        XCTAssertEqual(targets.items.count, 2)
        let first = targets.items[0]
        XCTAssertEqual(first.decisionKey, "decision-1")
        XCTAssertEqual(first.rowVersion, "7")
        XCTAssertEqual(first.digest, "digest-1")
        XCTAssertEqual(first.kind, .question)
        XCTAssertEqual(first.options, ["main", "rolling"])
        XCTAssertFalse(first.pending)
        XCTAssertNil(first.receipt)
        let pending = targets.items[1]
        XCTAssertTrue(pending.pending)
        XCTAssertNil(pending.rowVersion)
        XCTAssertEqual(pending.receipt, PendingReceipt(key: receiptKey, state: "queued"))
    }

    func testTargetsWithoutAConfiguredManagerOfferOnlyPendingRows() throws {
        let body = """
            {"manager":"none","items":[{"decision_key":"d","row_version":"1","target_digest":"x",
              "kind":"question","title":"t","detail":"","options":[]}],"truncated":true}
            """
        let targets = try parseTargets(Data(body.utf8))
        XCTAssertFalse(targets.ready)
        XCTAssertNil(targets.fence)
        XCTAssertTrue(targets.items.isEmpty)
        XCTAssertTrue(targets.truncated)
    }

    func testTargetsRefuseBadVersionsKindsAndPendingIdentity() {
        let badVersion = targetsBody.replacingOccurrences(of: #""row_version":"7""#, with: #""row_version":"07""#)
        XCTAssertThrowsError(try parseTargets(Data(badVersion.utf8)))
        let badKind = targetsBody.replacingOccurrences(of: #""kind":"question""#, with: #""kind":"quest""#)
        XCTAssertThrowsError(try parseTargets(Data(badKind.utf8)))
        let badPending = targetsBody.replacingOccurrences(of: "pending-approval:abc", with: "pending-question:abc")
        XCTAssertThrowsError(try parseTargets(Data(badPending.utf8)))
        let badReceipt = targetsBody.replacingOccurrences(of: #""queued""#, with: #""done""#)
        XCTAssertThrowsError(try parseTargets(Data(badReceipt.utf8)))
        XCTAssertThrowsError(try parseTargets(Data("[]".utf8)))
    }

    func testAnApprovalWithoutOptionsIsNotAnswerable() throws {
        let body = """
            {"manager":"configured","fence":{"policy_version":"1","scope_version":"1"},
             "items":[{"decision_key":"d","row_version":"1","target_digest":"x",
              "kind":"approval","title":"t","detail":"","options":[]}]}
            """
        let item = try XCTUnwrap(try parseTargets(Data(body.utf8)).items.first)
        XCTAssertFalse(item.isAnswerable)
    }

    func testPendingReceiptsAreParsedStrictlyAndNeverClaimAnswered() throws {
        let ok = try parseAnswerReceipt(
            Data(#"{"state":"uncertain","receipt_key":"\#(receiptKey)"}"#.utf8), pending: true)
        XCTAssertEqual(ok, .pending(PendingReceipt(key: receiptKey, state: "uncertain")))
        XCTAssertThrowsError(try parseAnswerReceipt(Data("{}".utf8), pending: true)) { error in
            XCTAssertEqual((error as? AnswerError)?.kind, .unknown)
        }
        XCTAssertEqual(
            try parseAnswerReceipt(Data(#"{"key":"r-1","deduplicated":true}"#.utf8), pending: false),
            .delivered(key: "r-1", deduplicated: true))
        XCTAssertEqual(
            try parseAnswerReceipt(Data(), pending: false), .delivered(key: "", deduplicated: false))
        XCTAssertTrue(describePendingReceipt(PendingReceipt(key: receiptKey, state: "uncertain")).message.contains("may have been consumed"))
        XCTAssertEqual(describePendingReceipt(PendingReceipt(key: receiptKey, state: "failed")).tone, .error)
    }

    func testAnswerTextRules() {
        XCTAssertNotNil(answerProblem(""))
        XCTAssertNotNil(answerProblem("   \n"))
        XCTAssertNotNil(answerProblem("a\0b"))
        XCTAssertNil(answerProblem(String(repeating: "x", count: 2048)))
        XCTAssertNotNil(answerProblem(String(repeating: "x", count: 2049)))
        // Bytes, not characters: 1025 two-byte characters are 2050 bytes.
        XCTAssertNotNil(answerProblem(String(repeating: "é", count: 1025)))
        XCTAssertNil(answerProblem(String(repeating: "é", count: 1024)))
    }
}

// MARK: Request bodies

final class AnswerRequestTests: XCTestCase {
    func testDecisionAnswerBodyCarriesFenceVersionDigestAndKey() throws {
        let body = try json(decisionRequest(answer: "Yes").body())
        XCTAssertEqual(Set(body.keys), ["decision_key", "expected_row_version", "target_digest", "fence", "answer", "idempotency_key"])
        XCTAssertEqual(body["decision_key"] as? String, "decision-1")
        XCTAssertEqual(body["expected_row_version"] as? String, "7")
        XCTAssertEqual(body["target_digest"] as? String, "digest-1")
        XCTAssertEqual(body["answer"] as? String, "Yes")
        XCTAssertEqual(body["idempotency_key"] as? String, key1)
        let fence = try XCTUnwrap(body["fence"] as? [String: String])
        XCTAssertEqual(fence, ["policy_version": "3", "scope_version": "4"])
    }

    func testPendingAnswerBodyHasExactlyTheStrictFields() throws {
        let request = AnswerRequest(
            decisionKey: "pending-question:abc", pending: true, rowVersion: nil, digest: "digest-2",
            fence: nil, answer: "main", idempotencyKey: key1)
        let body = try json(request.body())
        XCTAssertEqual(Set(body.keys), ["decision_id", "expected_target_digest", "answer", "idempotency_key"])
        XCTAssertEqual(body["decision_id"] as? String, "pending-question:abc")
        XCTAssertEqual(body["expected_target_digest"] as? String, "digest-2")
        XCTAssertEqual(request.route, "answer-pending")
        XCTAssertEqual(decisionRequest().route, "answer")
    }

    func testInvalidRequestsAreRefusedBeforeAnythingIsSent() {
        for bad in [
            decisionRequest(answer: ""),
            decisionRequest(answer: "a\0b"),
            decisionRequest(answer: String(repeating: "x", count: 2049)),
            decisionRequest(key: "NOT-A-UUID"),
            decisionRequest(key: "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA"),
            AnswerRequest(decisionKey: "d", pending: false, rowVersion: "0", digest: "x",
                fence: AnswerFence(policyVersion: "1", scopeVersion: "1"), answer: "a", idempotencyKey: key1),
            AnswerRequest(decisionKey: "d", pending: false, rowVersion: "1", digest: "x",
                fence: nil, answer: "a", idempotencyKey: key1),
        ] {
            XCTAssertThrowsError(try bad.body()) { error in
                XCTAssertEqual((error as? AnswerError)?.kind, .invalid)
            }
        }
    }
}

// MARK: Status mapping

final class AnswerStatusTests: XCTestCase {
    func testStatusesMapToOutcomesLikeTheWebClient() {
        XCTAssertEqual(answerRefusal(status: 409, code: "", retryAfter: nil).kind, .stale)
        XCTAssertEqual(answerRefusal(status: 403, code: "", retryAfter: nil).kind, .denied)
        let limited = answerRefusal(status: 429, code: "", retryAfter: 7)
        XCTAssertEqual(limited.kind, .rateLimited)
        XCTAssertEqual(limited.retryAfter, 7)
        XCTAssertTrue(limited.message.contains("7 seconds"))
        let busy = answerRefusal(status: 503, code: "", retryAfter: nil)
        XCTAssertEqual(busy.kind, .busy)
        XCTAssertTrue(busy.message.contains("a moment"))
        XCTAssertEqual(answerRefusal(status: 400, code: "", retryAfter: nil).kind, .invalid)
        XCTAssertEqual(answerRefusal(status: 401, code: "", retryAfter: nil).kind, .session)
        XCTAssertEqual(answerRefusal(status: 500, code: "", retryAfter: nil).kind, .unexpected)
    }

    func testOnlyOutcomeUnknownBadGatewaysAreUnknown() {
        for status in [502, 504] {
            let unknown = answerRefusal(status: status, code: "outcome_unknown", retryAfter: nil)
            XCTAssertEqual(unknown.kind, .unknown)
            XCTAssertTrue(unknown.message.contains("not known whether your answer was delivered"))
            XCTAssertEqual(answerRefusal(status: status, code: "", retryAfter: nil).kind, .unavailable)
        }
    }

    func testRetryAfterIsClampedToWholeSeconds() {
        XCTAssertEqual(retryAfterSeconds("7"), 7)
        XCTAssertEqual(retryAfterSeconds("99999"), 3600)
        XCTAssertNil(retryAfterSeconds("0"))
        XCTAssertNil(retryAfterSeconds("soon"))
        XCTAssertNil(retryAfterSeconds(nil))
    }
}

// MARK: Client against the stub gateway

final class AnswerClientTests: XCTestCase {
    override func setUp() {
        super.setUp()
        StubGateway.shared.reset()
    }

    func testAnswerPostsWithCsrfOriginAndJsonBody() async throws {
        signIn()
        StubGateway.shared.script("POST \(basePath)/decisions/answer", .json(#"{"key":"r-1","deduplicated":false}"#))
        let receipt = try await stubbedClient().answer(
            project: projectId, session: sessionId, request: decisionRequest())
        XCTAssertEqual(receipt, .delivered(key: "r-1", deduplicated: false))
        let post = try XCTUnwrap(StubGateway.shared.requests.last)
        XCTAssertEqual(post.path, "\(basePath)/decisions/answer")
        XCTAssertEqual(post.headers["x-rsi-csrf"], "csrf-1")
        XCTAssertEqual(post.headers["origin"], "https://hub.example.ts.net")
        XCTAssertEqual(post.headers["content-type"], "application/json")
        XCTAssertEqual(try json(post.body)["idempotency_key"] as? String, key1)
    }

    func testPendingAnswerUsesTheAnswerPendingRoute() async throws {
        signIn()
        StubGateway.shared.script(
            "POST \(basePath)/decisions/answer-pending",
            .json(#"{"state":"queued","receipt_key":"\#(receiptKey)"}"#))
        let request = AnswerRequest(
            decisionKey: "pending-question:abc", pending: true, rowVersion: nil, digest: "d",
            fence: nil, answer: "main", idempotencyKey: key1)
        let receipt = try await stubbedClient().answer(
            project: projectId, session: sessionId, request: request)
        XCTAssertEqual(receipt, .pending(PendingReceipt(key: receiptKey, state: "queued")))
    }

    func testA401ReBootstrapsOnceAndResendsUnderTheSameIdempotencyKey() async throws {
        let gateway = StubGateway.shared
        gateway.script("GET /auth/bootstrap", .json(#"{"nonce":"n-1"}"#))
        gateway.script("POST /auth/session", .json(#"{"csrf":"csrf-1"}"#), .json(#"{"csrf":"csrf-2"}"#))
        gateway.script(
            "POST \(basePath)/decisions/answer", .json("{}", status: 401),
            .json(#"{"key":"r-1","deduplicated":false}"#))
        let receipt = try await stubbedClient().answer(
            project: projectId, session: sessionId, request: decisionRequest())
        XCTAssertEqual(receipt, .delivered(key: "r-1", deduplicated: false))
        let posts = gateway.requests.filter { $0.path == "\(basePath)/decisions/answer" }
        XCTAssertEqual(posts.count, 2)
        XCTAssertEqual(posts[0].body, posts[1].body)
        XCTAssertEqual(try json(posts[1].body)["idempotency_key"] as? String, key1)
        XCTAssertEqual(posts[0].headers["x-rsi-csrf"], "csrf-1")
        XCTAssertEqual(posts[1].headers["x-rsi-csrf"], "csrf-2")
        XCTAssertEqual(gateway.requests.filter { $0.path == "/auth/session" }.count, 2)
    }

    func testASecond401IsASessionErrorWithNoThirdAttempt() async {
        signIn()
        StubGateway.shared.script("POST \(basePath)/decisions/answer", .json("{}", status: 401))
        do {
            _ = try await stubbedClient().answer(
                project: projectId, session: sessionId, request: decisionRequest())
            XCTFail("expected a refusal")
        } catch let error as AnswerError {
            XCTAssertEqual(error.kind, .session)
        } catch {
            XCTFail("unexpected \(error)")
        }
        XCTAssertEqual(StubGateway.shared.requests.filter { $0.path == "\(basePath)/decisions/answer" }.count, 2)
    }

    func testRefusalStatusesComeBackAsTypedErrors() async {
        let cases: [(StubGateway.Reply, AnswerErrorKind, Int?)] = [
            (.json("{}", status: 409), .stale, nil),
            (.json("{}", status: 403), .denied, nil),
            (.json("{}", status: 429, headers: ["Retry-After": "12"]), .rateLimited, 12),
            (.json("{}", status: 503, headers: ["Retry-After": "2"]), .busy, 2),
            (.json(#"{"error":"outcome_unknown"}"#, status: 504), .unknown, nil),
            (.json(#"{"error":"outcome_unknown"}"#, status: 502), .unknown, nil),
            (.json("", status: 502), .unavailable, nil),
        ]
        for (reply, kind, retry) in cases {
            StubGateway.shared.reset()
            signIn()
            StubGateway.shared.script("POST \(basePath)/decisions/answer", reply)
            do {
                _ = try await stubbedClient().answer(
                    project: projectId, session: sessionId, request: decisionRequest())
                XCTFail("expected \(kind)")
            } catch let error as AnswerError {
                XCTAssertEqual(error.kind, kind)
                XCTAssertEqual(error.retryAfter, retry)
            } catch {
                XCTFail("unexpected \(error)")
            }
            // Never auto-retried: one POST only.
            XCTAssertEqual(StubGateway.shared.requests.filter { $0.path == "\(basePath)/decisions/answer" }.count, 1)
        }
    }

    func testALostConnectionAfterSendingIsUnknownButAnUnreachableHostIsNot() async {
        signIn()
        StubGateway.shared.script("POST \(basePath)/decisions/answer", .fail(.networkConnectionLost))
        do {
            _ = try await stubbedClient().answer(
                project: projectId, session: sessionId, request: decisionRequest())
            XCTFail("expected a failure")
        } catch let error as AnswerError {
            XCTAssertEqual(error.kind, .unknown)
        } catch {
            XCTFail("unexpected \(error)")
        }
        StubGateway.shared.reset()
        signIn()
        StubGateway.shared.script("POST \(basePath)/decisions/answer", .fail(.cannotConnectToHost))
        do {
            _ = try await stubbedClient().answer(
                project: projectId, session: sessionId, request: decisionRequest())
            XCTFail("expected a failure")
        } catch let error as AnswerError {
            XCTAssertEqual(error.kind, .unavailable)
        } catch {
            XCTFail("unexpected \(error)")
        }
    }

    func testTargetsReadMapsOffAndErrors() async throws {
        signIn()
        StubGateway.shared.script("GET \(basePath)/decision-targets", .json("{}", status: 403))
        guard case .off = await stubbedClient().decisionTargets(project: projectId, session: sessionId) else {
            return XCTFail("403 should turn answers off")
        }
        StubGateway.shared.script("GET \(basePath)/decision-targets", .json("{}", status: 404))
        guard case .off = await stubbedClient().decisionTargets(project: projectId, session: sessionId) else {
            return XCTFail("404 should turn answers off")
        }
        StubGateway.shared.script("GET \(basePath)/decision-targets", .json("{}", status: 500))
        guard case .failed = await stubbedClient().decisionTargets(project: projectId, session: sessionId) else {
            return XCTFail("500 should be an error, not off")
        }
        StubGateway.shared.script(
            "GET \(basePath)/decision-targets",
            .json(#"{"manager":"configured","fence":{"policy_version":"3","scope_version":"4"},"items":[]}"#))
        guard case .ok(let targets) = await stubbedClient().decisionTargets(project: projectId, session: sessionId) else {
            return XCTFail("a valid read should parse")
        }
        XCTAssertFalse(targets.ready)
    }
}

// MARK: Draft store

@MainActor
final class AnswerDraftTests: XCTestCase {
    private func target(version: String = "7", digest: String = "digest-1") -> AnswerTarget {
        AnswerTarget(
            decisionKey: "decision-1", pending: false, rowVersion: version, digest: digest,
            kind: .question, title: "Which?", detail: "", options: ["a", "b"], receipt: nil)
    }

    private func store() -> AnswerDrafts {
        var counter = 0
        return AnswerDrafts(newKey: {
            counter += 1
            return counter == 1 ? key1 : key2
        })
    }

    private let fence = AnswerFence(policyVersion: "3", scopeVersion: "4")

    func testTheSameKeyIsReusedForAnUnknownRetryAndRenewedForNewText() throws {
        let drafts = store()
        drafts.ensure(project: projectId, session: sessionId, target: target())
        drafts.setText(session: sessionId, decision: "decision-1", "yes")
        let first = try drafts.begin(session: sessionId, decision: "decision-1", fence: fence).get()
        XCTAssertEqual(first.idempotencyKey, key1)
        drafts.settleError(
            session: sessionId, decision: "decision-1",
            error: AnswerError(.unknown, "not known"))
        let unknown = try XCTUnwrap(drafts.draft(session: sessionId, decision: "decision-1"))
        XCTAssertEqual(unknown.status, .unknown)
        XCTAssertTrue(unknown.view.canRetry)
        XCTAssertFalse(unknown.view.canEdit)
        // The retry resends the frozen request, key and fence included.
        let retry = try drafts.begin(session: sessionId, decision: "decision-1", fence: nil).get()
        XCTAssertEqual(retry, first)
    }

    func testEditingAfterARefusedSendGetsAFreshKey() throws {
        let drafts = store()
        drafts.ensure(project: projectId, session: sessionId, target: target())
        drafts.setText(session: sessionId, decision: "decision-1", "yes")
        _ = try drafts.begin(session: sessionId, decision: "decision-1", fence: fence).get()
        drafts.settleError(
            session: sessionId, decision: "decision-1",
            error: answerRefusal(status: 429, code: "", retryAfter: 5))
        drafts.setText(session: sessionId, decision: "decision-1", "no")
        let second = try drafts.begin(session: sessionId, decision: "decision-1", fence: fence).get()
        XCTAssertEqual(second.idempotencyKey, key2)
        XCTAssertEqual(second.answer, "no")
    }

    func testAStaleRefusalDiscardsTheDraftAndTheNextOneCarriesTheNotice() throws {
        let drafts = store()
        drafts.ensure(project: projectId, session: sessionId, target: target())
        drafts.setText(session: sessionId, decision: "decision-1", "yes")
        _ = try drafts.begin(session: sessionId, decision: "decision-1", fence: fence).get()
        drafts.settleError(
            session: sessionId, decision: "decision-1",
            error: answerRefusal(status: 409, code: "", retryAfter: nil))
        XCTAssertNil(drafts.draft(session: sessionId, decision: "decision-1"))
        let next = drafts.ensure(project: projectId, session: sessionId, target: target(version: "8"))
        XCTAssertTrue(next.notice.contains("changed"))
        XCTAssertEqual(next.text, "")
    }

    func testANewRowVersionDiscardsAnIdleDraft() {
        let drafts = store()
        drafts.ensure(project: projectId, session: sessionId, target: target())
        drafts.setText(session: sessionId, decision: "decision-1", "typed")
        let fresh = drafts.ensure(project: projectId, session: sessionId, target: target(version: "8"))
        XCTAssertEqual(fresh.text, "")
        XCTAssertTrue(fresh.notice.contains("discarded"))
    }

    func testPickingOptionsIsSingleOrMultiSelect() {
        let drafts = store()
        drafts.ensure(project: projectId, session: sessionId, target: target())
        drafts.pick(session: sessionId, decision: "decision-1", option: "a")
        drafts.pick(session: sessionId, decision: "decision-1", option: "b")
        XCTAssertEqual(drafts.draft(session: sessionId, decision: "decision-1")?.text, "b")
        let other = "session-2"
        drafts.ensure(project: projectId, session: other, target: target(), multiSelect: true)
        drafts.pick(session: other, decision: "decision-1", option: "a")
        XCTAssertEqual(drafts.draft(session: other, decision: "decision-1")?.text, "a")
        drafts.pick(session: other, decision: "decision-1", option: "b")
        XCTAssertEqual(drafts.draft(session: other, decision: "decision-1")?.text, "a, b")
        drafts.pick(session: other, decision: "decision-1", option: "a")
        XCTAssertEqual(drafts.draft(session: other, decision: "decision-1")?.text, "b")
    }

    func testBeginRefusesEmptyAnswersAndAMissingFence() {
        let drafts = store()
        drafts.ensure(project: projectId, session: sessionId, target: target())
        if case .success = drafts.begin(session: sessionId, decision: "decision-1", fence: fence) {
            XCTFail("an empty answer must not be sent")
        }
        drafts.setText(session: sessionId, decision: "decision-1", "yes")
        if case .success = drafts.begin(session: sessionId, decision: "decision-1", fence: nil) {
            XCTFail("a daemon decision needs the fence")
        }
    }

    func testSubmitDeliversThroughTheClientAndShowsTheReceipt() async throws {
        StubGateway.shared.reset()
        signIn()
        StubGateway.shared.script(
            "POST \(basePath)/decisions/answer", .json(#"{"key":"r-9","deduplicated":true}"#))
        let drafts = store()
        drafts.ensure(project: projectId, session: sessionId, target: target())
        drafts.setText(session: sessionId, decision: "decision-1", "yes")
        let status = await drafts.submit(
            client: stubbedClient(), session: sessionId, decision: "decision-1", fence: fence)
        XCTAssertEqual(status, .receipt)
        let view = try XCTUnwrap(drafts.draft(session: sessionId, decision: "decision-1")).view
        XCTAssertTrue(view.message.contains("r-9"))
        XCTAssertTrue(view.message.contains("already been recorded"))
        XCTAssertTrue(view.canDismiss)
    }

    func testSubmitWithAnUnknownOutcomeLocksTheDraftForASameKeyRetry() async throws {
        StubGateway.shared.reset()
        signIn()
        StubGateway.shared.script(
            "POST \(basePath)/decisions/answer",
            .json(#"{"error":"outcome_unknown"}"#, status: 504),
            .json(#"{"key":"r-1","deduplicated":true}"#))
        let drafts = store()
        let client = stubbedClient()
        drafts.ensure(project: projectId, session: sessionId, target: target())
        drafts.setText(session: sessionId, decision: "decision-1", "yes")
        let first = await drafts.submit(client: client, session: sessionId, decision: "decision-1", fence: fence)
        XCTAssertEqual(first, .unknown)
        let second = await drafts.submit(client: client, session: sessionId, decision: "decision-1", fence: fence)
        XCTAssertEqual(second, .receipt)
        let posts = StubGateway.shared.requests.filter { $0.path == "\(basePath)/decisions/answer" }
        XCTAssertEqual(posts.count, 2)
        XCTAssertEqual(posts[0].body, posts[1].body)
    }
}
