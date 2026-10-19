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
    /// 403: this device is not on the gateway's allowlist (or Remote is off).
    case forbidden
    /// 502: the gateway could not reach rsid.
    case daemonUnavailable
    /// 503: the host is busy; polling retries it.
    case busy
    /// Any other HTTP failure, with the gateway's error code when it sent one.
    case server(status: Int, code: String)
    /// A body this app could not read.
    case invalidResponse(String)

    var message: String {
        switch self {
        case .notConfigured:
            "Set the RSI host in Settings."
        case .invalidHost:
            "Enter the host name only, for example hub.your-tailnet.ts.net."
        case .unreachable(let detail):
            "Can't reach the RSI host. Check that Tailscale is on and connected. (\(detail))"
        case .unauthorized:
            "The gateway would not sign this app in. Pull to retry."
        case .forbidden:
            "This device is not allowed. Check Remote settings on the hub."
        case .daemonUnavailable:
            "The RSI daemon is unavailable. Check that rsid is running on the hub."
        case .busy:
            "The RSI host is busy; retrying…"
        case .server(let status, let code):
            code.isEmpty
                ? "The RSI host returned \(status)."
                : "The RSI host returned \(status) (\(code))."
        case .invalidResponse(let what):
            "The RSI host sent a reply this app could not read (\(what))."
        }
    }

    /// Network-level, as opposed to an answer from the server.
    var isUnreachable: Bool {
        if case .unreachable = self { true } else { false }
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

    let host: String
    private let session: URLSession
    private var csrf: String?

    /// `session` lets tests route the traffic through a `URLProtocol` stub.
    init(host: String, session: URLSession? = nil) {
        self.host = host
        if let session {
            self.session = session
            return
        }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpShouldSetCookies = true
        configuration.httpCookieAcceptPolicy = .always
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 15
        configuration.waitsForConnectivity = false
        self.session = URLSession(configuration: configuration)
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

    private func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch {
            if let reason = unreachableReason(error) { throw RemoteError.unreachable(reason) }
            throw RemoteError.unreachable(error.localizedDescription)
        }
        guard let http = response as? HTTPURLResponse else {
            throw RemoteError.invalidResponse("not HTTP")
        }
        guard data.count <= Self.maxBodyBytes else { throw RemoteError.invalidResponse("too large") }
        return (data, http)
    }

    private struct Bootstrap: Decodable { let nonce: String }
    private struct Issued: Decodable { let csrf: String }

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

    /// One read with its final status: signs in first if needed and once more on a 401.
    private func read(_ path: String) async throws -> (Data, HTTPURLResponse) {
        if csrf == nil { try await establishSession() }
        var (data, response) = try await send(try request(path))
        if response.statusCode == 401 {
            csrf = nil
            try await establishSession()
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

    func history(project: String, session id: String) async throws -> HistoryPageResult {
        try await get(HistoryPageResult.self, sessionPath(project, id) + "/history")
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
                throw MessageError(.session, "\(reason.message) Nothing was sent.")
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
        if csrf == nil {
            do {
                try await establishSession()
            } catch let error as RemoteError {
                throw .signIn(error)
            } catch {
                throw .signIn(.invalidResponse("session"))
            }
        }
        var (data, response) = try await post(path, body: body)
        if response.statusCode == 401 {
            csrf = nil
            do {
                try await establishSession()
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
        do {
            (data, response) = try await session.data(for: post)
        } catch {
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
