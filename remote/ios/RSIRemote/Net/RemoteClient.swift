import Foundation

/// What went wrong, in terms the operator can act on.
enum RemoteError: Error, Equatable, Sendable {
    /// No gateway host is set yet (Settings).
    case notConfigured
    /// The host text is not a bare hostname such as `hub.tailnet.ts.net`.
    case invalidHost
    /// The phone could not reach the host at all: Tailscale off, no network,
    /// unknown name or a timeout. Not a server error.
    case unreachable(String)
    /// Still 401 after signing in again.
    case unauthorized
    /// 403, still after one retry: this device is not on the gateway's
    /// allowlist (or Remote is off). A single bare 403 can also be the
    /// gateway's identity check refusing while tailscaled is busy (#350).
    case forbidden
    /// 502: the gateway could not reach rsid (it is down, mostly a deploy restart).
    case daemonUnavailable
    /// 503: rsid (or the gateway's read pool) is busy.
    case busy
    /// Any other HTTP failure, with the gateway's error code when it sent one.
    case server(status: Int, code: String)
    /// A body this app could not read.
    case invalidResponse(String)
    /// The read was given up because its screen went away (a tab switch, a
    /// navigation, a new pull): not a failure, never shown.
    case cancelled

    /// A few words naming the kind of failure, for status lines.
    var headline: String {
        switch self {
        case .notConfigured, .invalidHost: "No RSI host set"
        case .unreachable: "Can't reach the hub"
        case .unauthorized: "Sign-in failed"
        case .forbidden: "Refused by the hub"
        case .daemonUnavailable: "Hub restarting"
        case .busy: "Hub busy"
        case .server, .invalidResponse: "Hub error"
        case .cancelled: "Cancelled"
        }
    }

    var message: String {
        switch self {
        case .notConfigured:
            "Set the RSI host in Settings."
        case .invalidHost:
            "Enter the host name only, for example hub.your-tailnet.ts.net."
        case .unreachable(let detail) where detail == "timed out":
            "The RSI host did not answer within 15 s (timed out). The network (Tailscale) or the hub is slow."
        case .unreachable(let detail):
            "Can't reach the RSI host over the network (\(detail)). Check that Tailscale is on and connected."
        case .unauthorized:
            "The gateway would not sign this app in. Pull to retry."
        case .forbidden:
            "The hub refused this device (403). Check that this phone is allowed in the hub's Remote settings."
        case .daemonUnavailable:
            "The hub's daemon (rsid) is not answering; it is most likely restarting after a deploy."
        case .busy:
            "The hub's daemon is busy and could not answer in time."
        case .server(let status, let code):
            code.isEmpty
                ? "The RSI host returned \(status)."
                : "The RSI host returned \(status) (\(code))."
        case .invalidResponse(let what):
            "The RSI host sent a reply this app could not read (\(what))."
        case .cancelled:
            "Cancelled."
        }
    }

    /// Network-level, as opposed to an answer from the server.
    var isUnreachable: Bool {
        if case .unreachable = self { true } else { false }
    }

    /// Whether, and how often, a GET that ended in this error is tried again.
    var readRetry: RetryClass {
        switch self {
        case .busy, .daemonUnavailable: .always
        case .server(let status, _) where status == 504: .always
        // A bare 403 is retried once: the identity check also answers 403 when
        // tailscaled's LocalAPI is busy (hub log `localapi_busy`, #350).
        case .forbidden: .once
        // A 500 is not retried in place: on the hub every GetDecisions read
        // failed with one, fast, for minutes (#350 soak), and a retry only
        // doubled the load. The screen's backoff reads again.
        default: .never
        }
    }
}

/// How often a failed read may be tried again.
enum RetryClass: Sendable, Equatable {
    case never
    /// At most one retry for this kind of failure.
    case once
    /// Up to the read's attempt limit.
    case always
}

/// How a GET rides out a busy, restarting or briefly unreachable hub (#350):
/// a few attempts with jittered exponential backoff, honouring a short
/// `Retry-After`. Writes are never retried here (their rules are below).
struct ReadRetry: Sendable {
    /// Attempts in total, the first included.
    var attempts = 4
    /// The first backoff; each later one doubles, up to `maxDelay` seconds.
    var baseDelay = 0.5
    var maxDelay = 4.0
    /// A `Retry-After` longer than this ends the retries; the screen's own
    /// refresh comes back after it.
    var maxRetryAfter = 8
    /// Scales each backoff so many clients do not retry in step.
    var jitter: @Sendable () -> Double = { Double.random(in: 0.5...1) }
    var sleep: @Sendable (Double) async throws -> Void = { try await Task.sleep(for: .seconds($0)) }

    static let standard = ReadRetry()

    /// The wait before attempt `attempt + 1`, or nil to give up.
    func delay(after attempt: Int, retryAfter: Int?) -> Double? {
        guard attempt < attempts else { return nil }
        if let retryAfter, retryAfter > maxRetryAfter { return nil }
        let backoff = min(maxDelay, baseDelay * Double(1 << min(attempt - 1, 16))) * jitter()
        return max(backoff, Double(retryAfter ?? 0))
    }
}

/// Normalise what the operator typed into the gateway's canonical host:
/// a scheme and trailing slash are dropped, anything else is refused.
func normalizeHost(_ raw: String) -> String? {
    var text = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    if text.hasPrefix("https://") { text.removeFirst("https://".count) }
    while text.hasSuffix("/") { text.removeLast() }
    guard !text.isEmpty, text.count <= 253 else { return nil }
    let parts = text.split(separator: ":", omittingEmptySubsequences: false)
    guard parts.count <= 2 else { return nil }
    let name = parts[0]
    let allowed = Set("abcdefghijklmnopqrstuvwxyz0123456789.-")
    guard !name.isEmpty, name.allSatisfy({ allowed.contains($0) }),
        !name.hasPrefix("."), !name.hasPrefix("-"), !name.hasSuffix(".")
    else { return nil }
    if parts.count == 2 {
        guard let port = Int(parts[1]), (1...65535).contains(port) else { return nil }
    }
    return text
}

/// Whether a transport failure is worth one more try: the link or the
/// Tailscale path changed under the request. A timeout already waited 15 s and
/// is left to the screen's own refresh.
func transientTransport(_ code: URLError.Code) -> RetryClass {
    switch code {
    case .networkConnectionLost, .cannotConnectToHost, .notConnectedToInternet,
        .cannotFindHost, .dnsLookupFailed:
        .always
    case .secureConnectionFailed:
        .once
    default:
        .never
    }
}

/// True for the error a task gets when it, or its URLSession task, was cancelled.
func isCancellation(_ error: any Error) -> Bool {
    error is CancellationError || (error as? URLError)?.code == .cancelled
}

/// Routes the gateway serves from its 2-permit operator pool (v2 reads and
/// messages, decision targets, answers); every other route takes one of its 4
/// read permits (crates/rsi-remote/src/reads.rs DaemonReads).
func isOperatorRoute(_ path: String) -> Bool {
    path.hasPrefix("/api/v2/") || path.hasSuffix("/decision-targets") || path.contains("/decisions/answer")
}

/// Map a transport failure to "unreachable" (Tailscale off, no route, DNS,
/// timeout) or nil when it is something else.
func unreachableReason(_ error: any Error) -> String? {
    guard let urlError = error as? URLError else { return nil }
    switch urlError.code {
    case .notConnectedToInternet: return "no network"
    case .cannotFindHost, .dnsLookupFailed: return "host name not found"
    case .cannotConnectToHost: return "connection refused"
    case .timedOut: return "timed out"
    case .networkConnectionLost: return "connection lost"
    case .secureConnectionFailed, .serverCertificateUntrusted,
        .serverCertificateHasBadDate, .serverCertificateNotYetValid,
        .serverCertificateHasUnknownRoot:
        return "TLS failed"
    default: return nil
    }
}

/// Map an HTTP status (after the one 401 retry) to an operator-facing error.
func statusError(_ status: Int, code: String) -> RemoteError {
    switch status {
    case 401: .unauthorized
    case 403: .forbidden
    case 502: .daemonUnavailable
    case 503: .busy
    default: .server(status: status, code: code)
    }
}

/// The gateway's error code from `{"error": code}` or a V1 safe error body.
func errorCode(from body: Data) -> String {
    guard body.count <= 4096 else { return "" }
    if let object = try? JSONSerialization.jsonObject(with: body) as? [String: Any] {
        if let code = object["error"] as? String { return code }
        if let code = object["code"] as? String { return code }
    }
    return ""
}

/// One gateway connection. Tailnet identity is the authentication: the
/// gateway's WhoIs check admits this device, then the bootstrap nonce and
/// session cookie bind it. Nothing secret is stored; the cookie and CSRF
/// token live in memory only (an ephemeral session).
actor RemoteClient {
    static let maxBodyBytes = 1_048_576
    /// Requests in flight at once. The gateway refuses rather than queues: 4
    /// tailscaled identity checks (a 403) and 4 daemon reads (a 503), with
    /// none waiting, so the app stays below both (#350).
    static let maxInFlight = 3
    /// Requests in flight on the gateway's 2-permit operator pool.
    static let maxOperatorInFlight = 2

    let host: String
    private let session: URLSession
    private let retry: ReadRetry
    private var csrf: String?
    /// Bumped by each sign-in, so a 401 for an older session does not end a newer one.
    private var generation = 0
    /// The sign-in in progress; concurrent requests wait for this one.
    private var signingIn: Task<Void, any Error>?
    private let slots = AsyncLimiter(slots: RemoteClient.maxInFlight)
    private let operatorSlots = AsyncLimiter(slots: RemoteClient.maxOperatorInFlight)

    /// `session` lets tests route the traffic through a `URLProtocol` stub;
    /// `retry` lets them replace the backoff sleeps.
    init(host: String, session: URLSession? = nil, retry: ReadRetry = .standard) {
        self.host = host
        self.session = session ?? URLSession(configuration: Self.configuration())
        self.retry = retry
    }

    /// The production session settings: an ephemeral cookie jar, no cache.
    static func configuration() -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpShouldSetCookies = true
        configuration.httpCookieAcceptPolicy = .always
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 15
        configuration.waitsForConnectivity = false
        return configuration
    }

    private var origin: String { "https://\(host)" }

    private func url(_ path: String) throws -> URL {
        guard let url = URL(string: origin + path) else { throw RemoteError.invalidHost }
        return url
    }

    private func request(_ path: String, method: String = "GET") throws -> URLRequest {
        var request = URLRequest(url: try url(path))
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        // The gateway's exact-origin gate (crates/rsi-remote session::origin_ok).
        request.setValue(origin, forHTTPHeaderField: "Origin")
        return request
    }

    /// One HTTP exchange inside the in-flight limits. Throws only
    /// `RemoteError.cancelled`, when cancelled before it was sent; the
    /// exchange's own outcome is the result.
    private func exchange(_ request: URLRequest) async throws -> Result<(Data, URLResponse), any Error> {
        let operatorRoute = isOperatorRoute(request.url?.path ?? "")
        // The scarcer operator slot first, so a waiting operator request never
        // holds one of the general slots that plain reads need.
        if operatorRoute {
            do { try await operatorSlots.acquire() } catch { throw RemoteError.cancelled }
        }
        do {
            try await slots.acquire()
        } catch {
            if operatorRoute { await operatorSlots.release() }
            throw RemoteError.cancelled
        }
        let result: Result<(Data, URLResponse), any Error>
        do {
            result = .success(try await session.data(for: request))
        } catch {
            result = .failure(error)
        }
        await slots.release()
        if operatorRoute { await operatorSlots.release() }
        return result
    }

    /// A request that failed before any HTTP status arrived.
    private struct TransportFailure: Error {
        let code: URLError.Code?
        let error: RemoteError

        var retry: RetryClass { code.map(transientTransport) ?? .never }
    }

    private func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let data: Data
        let response: URLResponse
        switch try await exchange(request) {
        case .success(let reply):
            (data, response) = reply
        case .failure(let error):
            if isCancellation(error) { throw RemoteError.cancelled }
            throw TransportFailure(
                code: (error as? URLError)?.code,
                error: .unreachable(unreachableReason(error) ?? error.localizedDescription))
        }
        guard let http = response as? HTTPURLResponse else {
            throw RemoteError.invalidResponse("not HTTP")
        }
        guard data.count <= Self.maxBodyBytes else { throw RemoteError.invalidResponse("too large") }
        return (data, http)
    }

    private struct Bootstrap: Decodable { let nonce: String }
    private struct Issued: Decodable { let csrf: String }

    /// Make sure a session exists. Requests that need one at the same time
    /// share a single sign-in: the gateway's boot cookie is single-use and
    /// one cookie jar holds one at a time, so two interleaved sign-ins refuse
    /// each other with a 403 (#350).
    private func ensureSession() async throws {
        if csrf != nil { return }
        if let signingIn {
            try await signingIn.value
            return
        }
        let task = Task { try await self.establishSession() }
        signingIn = task
        defer { signingIn = nil }
        try await task.value
    }

    /// GET /auth/bootstrap, then POST /auth/session with its nonce.
    private func establishSession() async throws {
        let (bootData, boot) = try await send(try request("/auth/bootstrap"))
        guard boot.statusCode == 200 else {
            throw statusError(boot.statusCode, code: errorCode(from: bootData))
        }
        let nonce = try decode(Bootstrap.self, bootData, "bootstrap").nonce
        var post = try request("/auth/session", method: "POST")
        post.setValue("application/json", forHTTPHeaderField: "Content-Type")
        post.httpBody = try JSONEncoder().encode(["nonce": nonce])
        let (data, response) = try await send(post)
        guard response.statusCode == 200 else {
            throw statusError(response.statusCode, code: errorCode(from: data))
        }
        csrf = try decode(Issued.self, data, "session").csrf
        generation += 1
    }

    /// The session a 401 was answered for is gone: forget it, unless a newer
    /// sign-in has already replaced it.
    private func sessionRejected(_ generation: Int) {
        if generation == self.generation { csrf = nil }
    }

    private func decode<T: Decodable>(_ type: T.Type, _ data: Data, _ what: String) throws -> T {
        do {
            return try JSONDecoder().decode(type, from: data)
        } catch {
            throw RemoteError.invalidResponse(what)
        }
    }

    /// One private read: signs in first if needed and once more on a 401.
    func get<T: Decodable & Sendable>(_ type: T.Type, _ path: String) async throws -> T {
        try decode(type, try await getData(path), path)
    }

    /// The raw body of one private read, with the same sign-in rules as `get`.
    func getData(_ path: String) async throws -> Data {
        let (data, response) = try await read(path)
        guard response.statusCode == 200 else {
            throw statusError(response.statusCode, code: errorCode(from: data))
        }
        return data
    }

    /// One read with its final status. Signs in first if needed (one sign-in
    /// shared by concurrent reads) and once more on a 401. A busy, restarting
    /// or briefly unreachable hub, or a bare 403, is tried again under
    /// `retry` (#350); the last answer is returned for the caller to map.
    /// Throws `RemoteError` only, `.cancelled` when its task was cancelled.
    private func read(_ path: String) async throws -> (Data, HTTPURLResponse) {
        var attempt = 1
        var onceSpent = false
        while true {
            let outcome: Result<(Data, HTTPURLResponse), RemoteError>
            let retryClass: RetryClass
            var retryAfter: Int?
            do {
                let (data, response) = try await readOnce(path)
                if response.statusCode == 200 { return (data, response) }
                outcome = .success((data, response))
                retryClass = statusError(response.statusCode, code: errorCode(from: data)).readRetry
                retryAfter = retryAfterSeconds(response.value(forHTTPHeaderField: "Retry-After"))
            } catch let failure as TransportFailure {
                outcome = .failure(failure.error)
                retryClass = failure.retry
            } catch let error as RemoteError {
                // A refused or unreadable sign-in, or a cancelled read.
                outcome = .failure(error)
                retryClass = error.readRetry
            } catch {
                outcome = .failure(.invalidResponse("request"))
                retryClass = .never
            }
            if case .failure(.cancelled) = outcome { throw RemoteError.cancelled }
            let allowed = retryClass == .always || (retryClass == .once && !onceSpent)
            guard allowed, let delay = retry.delay(after: attempt, retryAfter: retryAfter) else {
                return try outcome.get()
            }
            if retryClass == .once { onceSpent = true }
            do {
                try Task.checkCancellation()
                try await retry.sleep(delay)
            } catch {
                throw RemoteError.cancelled
            }
            attempt += 1
        }
    }

    /// One attempt: the shared sign-in, the GET, and one fresh sign-in and
    /// resend on a 401.
    private func readOnce(_ path: String) async throws -> (Data, HTTPURLResponse) {
        try await ensureSession()
        let generation = self.generation
        var (data, response) = try await send(try request(path))
        if response.statusCode == 401 {
            sessionRejected(generation)
            try await ensureSession()
            (data, response) = try await send(try request(path))
        }
        return (data, response)
    }

    // MARK: V1 reads

    func info() async throws -> InfoResult {
        try await get(InfoResult.self, "/api/v1/info")
    }

    func projects() async throws -> ProjectListResult {
        try await get(ProjectListResult.self, "/api/v1/projects")
    }

    func sessions(project: String, cursor: String? = nil) async throws -> SessionListResult {
        var path = "/api/v1/projects/\(escape(project))/sessions"
        if let cursor { path += "?cursor=\(escape(cursor))" }
        return try await get(SessionListResult.self, path)
    }

    func session(project: String, session id: String) async throws -> SessionDetailResult {
        try await get(SessionDetailResult.self, sessionPath(project, id))
    }

    /// The newest page, or with `cursor` (the `next_cursor` token of the page
    /// before) the next older one.
    func history(project: String, session id: String, cursor: String? = nil) async throws -> HistoryPageResult {
        var path = sessionPath(project, id) + "/history"
        if let cursor { path += "?cursor=\(escape(cursor))" }
        return try await get(HistoryPageResult.self, path)
    }

    func decisions(project: String, session id: String) async throws -> DecisionListResult {
        try await get(DecisionListResult.self, sessionPath(project, id) + "/decisions")
    }

    // MARK: V1 answers

    /// `GET .../decision-targets`. 403 and 404 mean answers are off (the
    /// session stays read-only); any other failure is reported, not hidden.
    func decisionTargets(project: String, session id: String) async -> TargetsOutcome {
        do {
            let data = try await getData(sessionPath(project, id) + "/decision-targets")
            return .ok(try parseTargets(data))
        } catch RemoteError.forbidden {
            return .off
        } catch RemoteError.server(let status, _) where status == 404 {
            return .off
        } catch let error as RemoteError {
            return .failed(error)
        } catch {
            return .failed(.invalidResponse("decision targets"))
        }
    }

    /// POST one answer. The CSRF token from `/auth/session` rides in
    /// `x-rsi-csrf`. A 401 is denied before anything is dispatched, so exactly
    /// one re-bootstrap and resend under the same idempotency key is safe. A
    /// lost connection after sending is `.unknown`: never retried here.
    func answer(
        project: String, session id: String, request answer: AnswerRequest
    ) async throws -> AnswerReceipt {
        let body = try answer.body()
        let path = sessionPath(project, id) + "/decisions/" + answer.route
        let data: Data
        let response: HTTPURLResponse
        do throws(WriteFailure) {
            (data, response) = try await write(path, body: body)
        } catch {
            switch error {
            case .signIn(let reason):
                throw AnswerError(.session, "\(reason.message) Nothing was sent.")
            case .reSignIn:
                throw answerRefusal(status: 401, code: "", retryAfter: nil)
            case .unreadable:
                throw AnswerError(
                    .unknown,
                    "The RSI host sent a reply this app could not read, so it is not known whether your answer was delivered."
                )
            case .neverSent:
                throw AnswerError(
                    .unavailable,
                    "Can't reach the RSI host. Nothing was sent. Check that Tailscale is on and try again."
                )
            case .lost:
                throw AnswerError(
                    .unknown,
                    "Lost the connection while sending, so it is not known whether your answer was delivered."
                )
            }
        }
        guard (200..<300).contains(response.statusCode) else {
            throw answerRefusal(
                status: response.statusCode, code: errorCode(from: data),
                retryAfter: retryAfterSeconds(response.value(forHTTPHeaderField: "Retry-After")))
        }
        return try parseAnswerReceipt(data, pending: answer.pending)
    }

    // MARK: V2 manager seats

    /// `GET /api/v2/projects`: the exposed projects with their manager seats.
    func managerSeats() async throws -> ProjectSeatsList {
        try await get(ProjectSeatsList.self, "/api/v2/projects")
    }

    /// `GET /api/v2/dashboard` (hub #3287): satellites and running agents.
    func dashboard() async throws -> Dashboard {
        try await get(Dashboard.self, "/api/v2/dashboard")
    }

    /// `GET /api/v2/providers` (hub #3796): every provider's plan windows,
    /// overage posture and spend. No query, no project.
    func providers() async throws -> ProviderUsage {
        try await get(ProviderUsage.self, "/api/v2/providers")
    }

    /// `GET …/conversation[?before=]`, newest first. `before` is a
    /// `next_before` the gateway gave, sent back unchanged (not percent-encoded:
    /// the gateway parses `<sequence>:<id>` literally).
    func conversation(project: String, seat: String, before: String? = nil) async throws(ReadFailure)
        -> ConversationPage
    {
        var path = managerPath(project, seat) + "/conversation"
        if let before {
            guard isConversationCursor(before) else {
                throw ReadFailure(error: .invalidResponse("conversation cursor"), retryAfter: nil)
            }
            path += "?before=" + before
        }
        do {
            let (data, response) = try await read(path)
            guard response.statusCode == 200 else {
                throw ReadFailure(
                    error: statusError(response.statusCode, code: errorCode(from: data)),
                    retryAfter: retryAfterSeconds(response.value(forHTTPHeaderField: "Retry-After")))
            }
            return try decode(ConversationPage.self, data, "conversation")
        } catch let failure as ReadFailure {
            throw failure
        } catch let error as RemoteError {
            throw ReadFailure(error: error, retryAfter: nil)
        } catch {
            throw ReadFailure(error: .invalidResponse("conversation"), retryAfter: nil)
        }
    }

    /// `POST …/messages` with the same write rules as `answer`: CSRF and exact
    /// Origin, one re-bootstrap on a 401 resending the identical body under
    /// the same key, no other automatic retry.
    func sendMessage(_ message: MessageRequest) async throws(MessageError) -> MessageReceipt {
        let body: Data
        do {
            body = try message.body()
        } catch {
            throw MessageError(.invalid, "This message is not valid. Nothing was sent.")
        }
        let data: Data
        let response: HTTPURLResponse
        do throws(WriteFailure) {
            (data, response) = try await write(message.path, body: body)
        } catch {
            switch error {
            case .signIn(let reason):
                // An unreachable host is the network, not a refused sign-in.
                throw MessageError(reason.isUnreachable ? .unavailable : .session, "\(reason.message) Nothing was sent.")
            case .reSignIn:
                throw messageRefusal(status: 401, code: "", retryAfter: nil)
            case .unreadable:
                throw MessageError(
                    .unknown,
                    "Not known whether delivered: the RSI host's reply could not be read. Retry sends the same message under the same key.")
            case .neverSent:
                throw MessageError(
                    .unavailable,
                    "Can't reach the RSI host. Nothing was sent. Check that Tailscale is on and try again.")
            case .lost:
                throw MessageError(
                    .unknown,
                    "Not known whether delivered: the connection broke while sending. Retry sends the same message under the same key.")
            }
        }
        guard response.statusCode == 200 else {
            throw messageRefusal(
                status: response.statusCode, code: errorCode(from: data),
                retryAfter: retryAfterSeconds(response.value(forHTTPHeaderField: "Retry-After")))
        }
        guard let receipt = try? JSONDecoder().decode(MessageReceipt.self, from: data) else {
            throw MessageError(
                .unknown,
                "Not known whether delivered: the receipt could not be read. Retry sends the same message under the same key.",
                status: 200)
        }
        return receipt
    }

    // MARK: Writes

    /// Why a write produced no HTTP status to judge.
    enum WriteFailure: Error {
        /// Signing in before the first send failed; nothing was sent.
        case signIn(RemoteError)
        /// A 401, then signing in again failed; nothing was sent.
        case reSignIn
        /// A reply arrived but could not be read; the write may have happened.
        case unreadable
        /// The connection failed before any byte left; nothing was sent.
        case neverSent
        /// The connection broke after sending may have started.
        case lost
    }

    /// One write with the CSRF token. On a 401 (denied before any dispatch)
    /// sign in again once and resend exactly `body`.
    private func write(_ path: String, body: Data) async throws(WriteFailure) -> (Data, HTTPURLResponse) {
        do {
            try await ensureSession()
        } catch let error as RemoteError {
            throw .signIn(error)
        } catch let failure as TransportFailure {
            throw .signIn(failure.error)
        } catch {
            throw .signIn(.invalidResponse("session"))
        }
        let generation = self.generation
        var (data, response) = try await post(path, body: body)
        if response.statusCode == 401 {
            sessionRejected(generation)
            do {
                try await ensureSession()
            } catch {
                throw .reSignIn
            }
            (data, response) = try await post(path, body: body)
        }
        return (data, response)
    }

    private func post(_ path: String, body: Data) async throws(WriteFailure) -> (Data, HTTPURLResponse) {
        guard var post = try? request(path, method: "POST") else { throw .neverSent }
        post.setValue("application/json", forHTTPHeaderField: "Content-Type")
        post.setValue(csrf, forHTTPHeaderField: "x-rsi-csrf")
        post.httpBody = body
        let data: Data
        let response: URLResponse
        let result: Result<(Data, URLResponse), any Error>
        do {
            result = try await exchange(post)
        } catch {
            // Cancelled while waiting for a slot: nothing left the phone.
            throw .neverSent
        }
        switch result {
        case .success(let reply):
            (data, response) = reply
        case .failure(let error):
            if let url = error as? URLError, Self.neverSent.contains(url.code) { throw .neverSent }
            // The request may have reached the gateway before the link broke.
            throw .lost
        }
        guard let http = response as? HTTPURLResponse, data.count <= Self.maxBodyBytes else {
            throw .unreadable
        }
        return (data, http)
    }

    /// Transport failures that happen before any byte of the request leaves.
    private static let neverSent: Set<URLError.Code> = [
        .notConnectedToInternet, .cannotFindHost, .dnsLookupFailed, .cannotConnectToHost,
    ]

    private func sessionPath(_ project: String, _ session: String) -> String {
        "/api/v1/projects/\(escape(project))/sessions/\(escape(session))"
    }
}

/// A failed read with the gateway's `Retry-After`, for the poll backoff.
struct ReadFailure: Error, Sendable, Equatable {
    let error: RemoteError
    let retryAfter: Int?
}

/// Percent-encode one path or query component.
func escape(_ component: String) -> String {
    var allowed = CharacterSet.alphanumerics
    allowed.insert(charactersIn: "-._~")
    return component.addingPercentEncoding(withAllowedCharacters: allowed) ?? component
}
