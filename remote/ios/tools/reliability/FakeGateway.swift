import Foundation
import Network

// A fault-injecting stand-in for the rsi-remote gateway (#350).
//
// The real gateway (crates/rsi-remote) serves only behind `tailscale serve`
// with tailscaled as a root peer, so it cannot run on a Mac without that. This
// emulator reproduces the parts of its behaviour that decide what the phone
// sees, taken from its code, and adds switchable faults:
//
// - sign-in: GET /auth/bootstrap sets a single-use `__Host-rsi_boot` cookie and
//   returns its nonce; POST /auth/session consumes the presented boot cookie
//   whatever the nonce, and any failure is a bare 403 (session.rs, main.rs route).
// - the caller-identity check: each request takes one of 4 LocalAPI permits
//   without waiting; none free is a bare 403 `localapi_busy`
//   (localapi_client.rs PERMITS, gate.rs authorize, main.rs handle).
// - daemon reads: v1 reads take one of 4 permits, v2 reads and decision targets
//   one of 2, without waiting; none free is 503 {"error":"busy"} + Retry-After 2
//   (reads.rs DaemonReads, main.rs failed_read).
// - rsid busy (Store lock contention): 503 busy + Retry-After 2; rsid down (a
//   deploy restart): 502 {"error":"connect"}.
// - every response carries `connection: close`, like the gateway.
//
// Faults are set with `POST /__control` (JSON: the keys of `Faults`, plus
// `reset` to start over and `clear_log`); `GET /__log` returns the request log.
// Neither exists on the real gateway.
//
// usage: fake-gateway <port> <identity.p12> <p12 password>

struct Faults {
    var readMs = 40.0  // daemon read latency
    var whoisMs = 5.0  // LocalAPI whois latency per request
    var busyNext = 0  // the next N daemon reads answer 503 busy (rsid contention)
    var forbidNext = 0  // the next N requests answer a bare 403 (localapi_busy)
    var downUntil = Date.distantPast  // daemon reads answer 502 connect until then
    var dropNext = 0  // the next N requests are closed with no response
    var stallNext = 0  // the next N daemon reads take stallS
    var stallS = 20.0
    var projects = 3
    var waiting = 2  // waiting sessions per project
    var answers = true  // decision targets served (false: the 403 of answers off)
    var loseReplyNext = 0  // the next N messages are queued, then the reply is dropped (a link that broke after the send)
    var messageBusyNext = 0  // the next N messages answer 503 busy + Retry-After 2, nothing queued
    var conversation = 40  // items in each seat's conversation (the chat view, #502)
    var appended = 0  // items added since: `append` streams new manager output

    mutating func apply(_ changes: [String: Any]) throws {
        for (key, value) in changes {
            let number = (value as? NSNumber)?.doubleValue ?? 0
            switch key {
            case "read_ms": readMs = number
            case "whois_ms": whoisMs = number
            case "busy_next": busyNext = Int(number)
            case "forbid_next": forbidNext = Int(number)
            case "down_for_s": downUntil = Date().addingTimeInterval(number)
            case "drop_next": dropNext = Int(number)
            case "stall_next": stallNext = Int(number)
            case "stall_s": stallS = number
            case "projects": projects = Int(number)
            case "waiting": waiting = Int(number)
            case "answers": answers = (value as? Bool) ?? true
            case "lose_reply_next": loseReplyNext = Int(number)
            case "message_busy_next": messageBusyNext = Int(number)
            case "conversation": conversation = Int(number)
            case "append": appended += Int(number)
            case "wipe": break
            default: throw ControlError(key: key)
            }
        }
    }
}

struct ControlError: Error { let key: String }

/// A message the daemon queued.
struct StoredMessage {
    let id: String
    let seat: String
    let key: String
    let text: String
}

struct Request {
    let method: String
    let path: String
    let headers: [String: String]
    let body: Data
    var query = ""

    func cookie(_ name: String) -> String? {
        var found: String?
        for part in (headers["cookie"] ?? "").split(separator: ";") {
            let pair = part.trimmingCharacters(in: .whitespaces).split(separator: "=", maxSplits: 1)
            guard pair.count == 2, pair[0] == name, !pair[1].isEmpty else { continue }
            if found != nil { return nil }
            found = String(pair[1])
        }
        return found
    }
}

struct Response {
    var status: Int
    var json: Any?
    var cookies: [String] = []
    var retryAfter: Int?
    var note = ""

    var bytes: Data {
        let reasons = [
            200: "OK", 400: "Bad Request", 401: "Unauthorized", 403: "Forbidden", 404: "Not Found",
            409: "Conflict", 429: "Too Many Requests", 502: "Bad Gateway", 503: "Service Unavailable",
            504: "Gateway Timeout",
        ]
        let reason = reasons[status] ?? "Error"
        let body: Data
        let type: String
        if let json, let data = try? JSONSerialization.data(withJSONObject: json) {
            body = data
            type = "application/json"
        } else {
            body = Data((reason.lowercased() + "\n").utf8)
            type = "text/plain; charset=utf-8"
        }
        var head = "HTTP/1.1 \(status) \(reason)\r\ncontent-type: \(type)\r\n"
        for cookie in cookies { head += "set-cookie: \(cookie)\r\n" }
        if let retryAfter { head += "retry-after: \(retryAfter)\r\n" }
        head += "content-length: \(body.count)\r\ncache-control: no-store\r\nconnection: close\r\n\r\n"
        return Data(head.utf8) + body
    }
}

/// A permit pool that never waits, like the gateway's `try_acquire`.
final class Permits: @unchecked Sendable {
    private let lock = NSLock()
    private var free: Int

    init(_ count: Int) { free = count }

    func tryAcquire() -> Bool {
        lock.withLock {
            guard free > 0 else { return false }
            free -= 1
            return true
        }
    }

    func release() { lock.withLock { free += 1 } }
}

final class Gateway: @unchecked Sendable {
    private let lock = NSLock()
    private var faults = Faults()
    private var boots: [String: String] = [:]
    private var sessions: [String: String] = [:]  // session cookie -> its CSRF token
    private var stored: [String: StoredMessage] = [:]  // "seat|key" -> the queued row
    private var lastMessageAt = Date.distantPast
    private var postCount = 0
    private var log: [[String: Any]] = []
    private var started = Date()
    private var readPermits = Permits(4)
    private var operatorPermits = Permits(2)
    private var whoisPermits = Permits(4)

    private func locked<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }

    func control(_ request: Request) -> Response {
        guard let changes = (try? JSONSerialization.jsonObject(with: request.body)) as? [String: Any] else {
            return Response(status: 400, json: ["error": "bad control"])
        }
        do {
            try locked {
                if changes["clear_log"] as? Bool == true { log = [] }
                if changes["reset"] as? Bool == true {
                    faults = Faults()
                    boots = [:]
                    sessions = [:]
                    stored = [:]
                    postCount = 0
                    lastMessageAt = .distantPast
                    log = []
                    started = Date()
                    readPermits = Permits(4)
                    operatorPermits = Permits(2)
                    whoisPermits = Permits(4)
                }
                if changes["wipe"] as? Bool == true {
                    boots = [:]
                    sessions = [:]
                }
                try faults.apply(changes.filter { !["reset", "clear_log"].contains($0.key) })
            }
        } catch {
            return Response(status: 400, json: ["error": "bad control: \(error)"])
        }
        return Response(status: 200, json: ["ok": true])
    }

    func logBody() -> Response {
        Response(status: 200, json: ["entries": locked { log }])
    }

    func record(_ request: Request, _ status: Int, _ note: String) {
        locked {
            log.append([
                "t": Date().timeIntervalSince(started), "method": request.method, "path": request.path,
                "status": status, "note": note,
            ])
        }
    }

    /// Take one unit of a counted fault; true when it applied.
    private func take(_ key: WritableKeyPath<Faults, Int>) -> Bool {
        locked {
            guard faults[keyPath: key] > 0 else { return false }
            faults[keyPath: key] -= 1
            return true
        }
    }

    private func pause(_ milliseconds: Double) async {
        try? await Task.sleep(for: .milliseconds(Int(milliseconds)))
    }

    /// The response, or nil to drop the connection without one.
    func handle(_ request: Request) async -> Response? {
        if take(\.dropNext) {
            record(request, 0, "dropped")
            return nil
        }
        // Caller identity: one LocalAPI whois per request, 4 permits, no wait.
        let whois = locked { whoisPermits }
        guard whois.tryAcquire() else { return Response(status: 403, note: "localapi_busy") }
        await pause(locked { faults.whoisMs })
        whois.release()
        if take(\.forbidNext) { return Response(status: 403, note: "injected_forbidden") }
        switch (request.method, request.path) {
        case ("GET", "/auth/bootstrap"):
            let cookie = Self.secret()
            let nonce = Self.secret()
            locked { boots[cookie] = nonce }
            return Response(
                status: 200, json: ["nonce": nonce],
                cookies: ["__Host-rsi_boot=\(cookie); Max-Age=60; Path=/; Secure; HttpOnly; SameSite=Strict"])
        case ("POST", "/auth/session"):
            let nonce = ((try? JSONSerialization.jsonObject(with: request.body)) as? [String: Any])?["nonce"] as? String
            let boot = request.cookie("__Host-rsi_boot")
            // The boot entry is consumed on presentation whatever the nonce.
            let expected = locked { boot.flatMap { boots.removeValue(forKey: $0) } }
            guard let expected, nonce == expected else {
                return Response(status: 403, note: expected == nil ? "auth_session_unknown_boot" : "auth_session_bad_nonce")
            }
            let session = Self.secret()
            let csrf = Self.secret()
            locked { sessions[session] = csrf }
            return Response(
                status: 200, json: ["csrf": csrf],
                cookies: [
                    "__Host-rsi_remote=\(session); Path=/; Secure; HttpOnly; SameSite=Strict",
                    "__Host-rsi_boot=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Strict",
                ])
        default:
            break
        }
        guard request.path.hasPrefix("/api/"), request.method == "GET" || request.method == "POST" else {
            return Response(status: 404)
        }
        guard let session = request.cookie("__Host-rsi_remote"), let csrf = locked({ sessions[session] }) else {
            return Response(status: 401)
        }
        if request.method == "POST" { return await message(request, session: session, csrf: csrf) }
        let operatorRoute = request.path.hasPrefix("/api/v2/") || request.path.hasSuffix("/decision-targets")
        let permits = locked { operatorRoute ? operatorPermits : readPermits }
        guard permits.tryAcquire() else {
            return Response(status: 503, json: ["error": "busy"], retryAfter: 2, note: "gateway_permit")
        }
        defer { permits.release() }
        return await daemonRead(request.path, query: request.query)
    }

    /// `POST /api/v2/projects/{project}/managers/{seat}/messages`, as the real
    /// gateway serves it (crates/rsi-remote managers.rs, docs/rsi-remote/contracts-v2.md):
    /// the CSRF token, 1 message per second, the seat check, and the daemon's
    /// `QueueOperatorMessage` idempotency per seat and key (the same key and text
    /// replay the stored row, other text is 409 idempotency_conflict).
    private func message(_ request: Request, session: String, csrf: String) async -> Response? {
        let parts = request.path.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
        guard parts.count == 8, parts[3] == "projects", parts[5] == "managers", parts[7] == "messages" else {
            return Response(status: 404)
        }
        guard request.headers["x-rsi-csrf"] == csrf else { return Response(status: 403, note: "bad_csrf") }
        let faults = locked { self.faults }
        let projectIds = (0..<faults.projects).map { Self.uuid(0x55, $0) }
        let seats = (0..<faults.projects).map { Self.uuid(0x77, $0) }
        guard projectIds.contains(parts[4]), seats.contains(parts[6]) else {
            return Response(status: 403, note: "not_a_seat")
        }
        let operatorPermit = locked { operatorPermits }
        guard operatorPermit.tryAcquire() else {
            return Response(status: 503, json: ["error": "busy"], retryAfter: 2, note: "gateway_permit")
        }
        defer { operatorPermit.release() }
        guard let body = (try? JSONSerialization.jsonObject(with: request.body)) as? [String: Any],
            body.count == 2, let text = body["text"] as? String, let key = body["idempotency_key"] as? String,
            !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, text.utf8.count <= 4096,
            key == key.lowercased(), UUID(uuidString: key) != nil
        else { return Response(status: 400, json: ["error": "invalid_message"], note: "invalid_message") }
        let now = Date()
        let tooFast = locked { () -> Bool in
            postCount += 1
            if now.timeIntervalSince(lastMessageAt) < 1 { return true }
            lastMessageAt = now
            return false
        }
        if tooFast { return Response(status: 429, json: ["error": "rate_limited"], retryAfter: 1, note: "rate_limited") }
        if take(\.messageBusyNext) {
            return Response(status: 503, json: ["error": "busy"], retryAfter: 2, note: "daemon_busy")
        }
        await pause(faults.readMs)
        let slot = "\(parts[6])|\(key)"
        let (row, replay, conflict) = locked { () -> (StoredMessage, Bool, Bool) in
            if let known = stored[slot] { return (known, true, known.text != text) }
            let row = StoredMessage(id: Self.uuid(0x88, stored.count + 1), seat: parts[6], key: key, text: text)
            stored[slot] = row
            return (row, false, false)
        }
        if conflict {
            return Response(status: 409, json: ["error": "idempotency_conflict"], note: "idempotency_conflict")
        }
        if take(\.loseReplyNext) {
            // Queued, then the link broke: the phone never sees the receipt.
            record(request, 0, replay ? "reply_lost_replay" : "reply_lost")
            return nil
        }
        return Response(
            status: 200,
            json: [
                "message_id": row.id, "seat_session_id": row.seat, "state": "queued", "created_at": Self.now(),
                "deduplicated": replay,
            ], note: replay ? "replayed" : "queued")
    }

    /// What the daemon holds: one row per seat and key, with how many POSTs reached the gateway.
    func messagesBody() -> Response {
        let rows = locked { stored.values.sorted { $0.id < $1.id } }.map {
            ["id": $0.id, "seat": $0.seat, "key": $0.key, "text": $0.text]
        }
        return Response(status: 200, json: ["messages": rows, "posts": locked { postCount }])
    }

    private func daemonRead(_ path: String, query: String = "") async -> Response {
        let faults = locked { self.faults }
        if Date() < faults.downUntil {
            return Response(status: 502, json: ["error": "connect"], note: "daemon_down")
        }
        if take(\.busyNext) {
            await pause(100)
            return Response(status: 503, json: ["error": "busy"], retryAfter: 2, note: "daemon_busy")
        }
        if take(\.stallNext) { await pause(faults.stallS * 1000) }
        await pause(faults.readMs)
        let parts = path.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
        let projectIds = (0..<faults.projects).map { Self.uuid(0x55, $0) }
        switch true {
        case path == "/api/v1/info":
            return ok([
                "version": "1",
                "item": ["protocol": "rsi-remote-v1", "daemon_boot_id": UUID().uuidString.lowercased(), "required_capabilities": []],
            ])
        case path == "/api/v1/projects":
            let items = projectIds.enumerated().map { ["id": $1, "name": "Project \($0 + 1)"] }
            return ok([
                "version": "1", "observed_at": Self.now(), "next_cursor": NSNull(), "complete": true, "degraded": [],
                "items": items,
            ])
        case parts.count == 6 && parts[5] == "sessions":
            guard let index = projectIds.firstIndex(of: parts[4]) else { return Response(status: 404) }
            return ok(sessionsBody(project: parts[4], index: index, waiting: faults.waiting))
        case parts.count == 8 && parts[7] == "decisions":
            return ok(decisionsBody(project: parts[4], session: parts[6]))
        case parts.count == 8 && parts[7] == "history":
            let event: [String: Any] = [
                "id": "1", "sequence": 1, "kind": Self.tagged("Message"), "role": Self.tagged("Assistant"),
                "created_at": Self.now(), "text": "Working on it.", "truncated": false, "tool_name": NSNull(),
            ]
            return ok(["complete": true, "next_cursor": NSNull(), "items": [event]])
        case parts.count == 8 && parts[5] == "managers" && parts[7] == "conversation":
            let before = query.hasPrefix("before=") ? String(query.dropFirst(7)) : nil
            guard let index = projectIds.firstIndex(of: parts[4]), parts[6] == Self.uuid(0x77, index) else {
                return Response(status: 404, json: ["error": "unknown_seat"])
            }
            return ok(conversationBody(project: parts[4], seat: parts[6], total: faults.conversation + faults.appended, before: before))
        case parts.count == 8 && parts[7] == "decision-targets":
            guard faults.answers else { return Response(status: 403, note: "answers_off") }
            return ok([
                "manager": "configured", "fence": ["policy_version": "3", "scope_version": "4"], "items": [],
            ])
        case path == "/api/v2/projects":
            let projects = projectIds.enumerated().map { index, id -> [String: Any] in
                let seat: [String: Any] = [
                    "last_activity_at": Self.now(), "model": "claude-opus-5-5", "pending_decisions": 1,
                    "pending_decisions_truncated": false, "provider": "Claude", "role": "project_manager",
                    "session_id": Self.uuid(0x77, index), "status": "Running",
                ]
                return ["project_id": id, "name": "Project \(index + 1)", "project_manager": seat, "global_manager": NSNull()]
            }
            return ok(["projects": projects, "messages_enabled": true])
        case path == "/api/v2/dashboard":
            let satellite: [String: Any] = [
                "build_sha": NSNull(), "label": "mac-mini", "last_seen_at": Self.now(), "reachable": true,
                "running_agents": 2,
            ]
            return ok([
                "agents": [["effort": "high", "provider": "Claude", "running": 4]], "agents_incomplete": false,
                "agents_total": 4, "agents_truncated": false, "observed_at": Self.now(), "satellites": [satellite],
                "satellites_truncated": false,
            ])
        default:
            return Response(status: 404)
        }
    }

    private func ok(_ json: [String: Any]) -> Response { Response(status: 200, json: json) }

    private func sessionsBody(project: String, index: Int, waiting: Int) -> [String: Any] {
        let items = (0..<(waiting + 2)).map { s -> [String: Any] in
            [
                "id": Self.uuid(0x66, index * 100 + s), "project_id": project, "parent_id": NSNull(),
                "continued_from": NSNull(), "kind": Self.tagged("Worker"), "own_title": "Worker \(s + 1)",
                "provider": Self.tagged("Claude"), "status": Self.tagged(s < waiting ? "WaitingApproval" : "Running"),
                "updated_at": Self.now(), "attention": ["requires_local_action": s < waiting, "incomplete": false],
            ]
        }
        return [
            "next_cursor": NSNull(), "complete": true, "degraded": [],
            "project": ["id": project, "name": "Project \(index + 1)"], "items": items,
        ]
    }

    private func decisionsBody(project: String, session: String) -> [String: Any] {
        let question: [String: Any] = [
            "header": NSNull(), "question": "Proceed with the change?",
            "options": [["label": "Yes", "description": NSNull()], ["label": "No", "description": NSNull()]],
            "multi_select": false,
        ]
        let item: [String: Any] = [
            "id": "decision-\(session.suffix(4))", "kind": "generic_questions", "closure_state": "open",
            "display": ["kind": "generic_questions"], "questions": [question], "requires_local_action": true,
        ]
        return ["complete": true, "project_id": project, "session_id": session, "items": [item]]
    }

    static func tagged(_ value: String) -> [String: Any] { ["state": "known", "value": value] }

    static func uuid(_ prefix: Int, _ n: Int) -> String {
        String(format: "%02x0e8400-e29b-41d4-a716-%012x", prefix, n)
    }

    static func secret() -> String {
        (0..<32).map { _ in String(format: "%02x", UInt8.random(in: 0...255)) }.joined()
    }

    static func now() -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.string(from: Date())
    }
}

// MARK: HTTP over Network.framework

final class Server: @unchecked Sendable {
    let gateway = Gateway()
    let queue = DispatchQueue(label: "fake-gateway", attributes: .concurrent)

    func serve(_ connection: NWConnection) {
        connection.start(queue: queue)
        receive(connection, Data())
    }

    private func receive(_ connection: NWConnection, _ buffer: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { data, _, done, error in
            var buffer = buffer
            if let data { buffer.append(data) }
            if let request = Self.parse(buffer) {
                Task { await self.answer(request, on: connection) }
            } else if done || error != nil {
                connection.cancel()
            } else {
                self.receive(connection, buffer)
            }
        }
    }

    private func answer(_ request: Request, on connection: NWConnection) async {
        let response: Response?
        switch request.path {
        case "/__control": response = gateway.control(request)
        case "/__log": response = gateway.logBody()
        case "/__messages": response = gateway.messagesBody()
        default:
            response = await gateway.handle(request)
            if let response { gateway.record(request, response.status, response.note) }
        }
        guard let response else {
            connection.cancel()
            return
        }
        connection.send(content: response.bytes, completion: .contentProcessed { _ in connection.cancel() })
    }

    /// A whole request (headers plus `content-length` body), or nil while incomplete.
    static func parse(_ buffer: Data) -> Request? {
        guard let end = buffer.range(of: Data("\r\n\r\n".utf8)) else { return nil }
        let head = String(decoding: buffer[..<end.lowerBound], as: UTF8.self)
        var lines = head.components(separatedBy: "\r\n")
        let start = lines.removeFirst().split(separator: " ")
        guard start.count >= 2 else { return nil }
        var headers: [String: String] = [:]
        for line in lines {
            guard let colon = line.firstIndex(of: ":") else { continue }
            headers[line[..<colon].lowercased()] = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
        }
        let length = Int(headers["content-length"] ?? "0") ?? 0
        let body = buffer[end.upperBound...]
        guard body.count >= length else { return nil }
        let target = String(start[1])
        let split = target.split(separator: "?", maxSplits: 1)
        let path = String(split.first ?? "")
        let query = split.count == 2 ? String(split[1]) : ""
        return Request(
            method: String(start[0]), path: path, headers: headers, body: Data(body.prefix(length)), query: query)
    }
}

@main
struct FakeGatewayMain {
    static func main() throws {
        let arguments = CommandLine.arguments
        guard arguments.count == 4, let port = NWEndpoint.Port(arguments[1]) else {
            FileHandle.standardError.write(Data("usage: fake-gateway <port> <identity.p12> <password>\n".utf8))
            exit(2)
        }
        let p12 = try Data(contentsOf: URL(fileURLWithPath: arguments[2]))
        var items: CFArray?
        let status = SecPKCS12Import(p12 as CFData, [kSecImportExportPassphrase: arguments[3]] as CFDictionary, &items)
        guard status == errSecSuccess, let first = (items as? [[String: Any]])?.first,
            let identity = first[kSecImportItemIdentity as String]
        else {
            FileHandle.standardError.write(Data("fake-gateway: cannot import the identity (\(status))\n".utf8))
            exit(1)
        }
        let tls = NWProtocolTLS.Options()
        sec_protocol_options_set_local_identity(
            tls.securityProtocolOptions, sec_identity_create(identity as! SecIdentity)!)
        let parameters = NWParameters(tls: tls)
        parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: port)
        let listener = try NWListener(using: parameters)
        let server = Server()
        listener.newConnectionHandler = { server.serve($0) }
        listener.stateUpdateHandler = { state in
            if case .ready = state { print("fake-gateway: https://localhost:\(port)") }
            if case .failed(let error) = state {
                FileHandle.standardError.write(Data("fake-gateway: \(error)\n".utf8))
                exit(1)
            }
        }
        listener.start(queue: server.queue)
        setvbuf(stdout, nil, _IOLBF, 0)
        dispatchMain()
    }
}

// MARK: Conversation (the chat view, #502)

extension Gateway {
    /// Item `n` (1-based) of a seat's scripted conversation: the operator asks every
    /// third item, the manager answers in Markdown; a few answers are long.
    static func conversationItem(_ n: Int) -> [String: Any] {
        let speaker = n % 3 == 1 ? "operator" : "manager"
        var text: String
        if speaker == "operator" {
            text = "Question \(n): how is the migration going, and what is next?"
        } else if n % 6 == 2 {
            text = "**Status \(n).** Three things landed:\n\n- the schema pin\n- the lander fix\n- `scoped_test` now reports the receipt head\n\nNext:\n\n1. merge the queue\n2. run the full gate\n\n```\nmake test-fast\n```"
        } else if n % 6 == 5 {
            text = (1...12).map { "Paragraph \($0) of a long answer \(n): the quick brown fox jumps over the lazy dog while the hub keeps queueing work." }.joined(separator: "\n\n")
        } else {
            text = "Update \(n): working on it; nothing needs you right now."
        }
        return [
            "id": String(900 + n), "sequence": n, "speaker": speaker, "text": text, "truncated": false,
            "created_at": now(), "operator_message_id": NSNull(),
        ]
    }

    /// `GET .../managers/{seat}/conversation[?before=<sequence>:<id>]`: 25 items, newest first.
    func conversationBody(project: String, seat: String, total: Int, before: String?) -> [String: Any] {
        var newest = total
        if let before, let sequence = Int(before.split(separator: ":").first ?? "") { newest = min(total, sequence - 1) }
        let first = max(1, newest - 24)
        let items = newest >= 1 ? (first...newest).reversed().map { Self.conversationItem($0) } : []
        let more = first > 1
        let seatInfo: [String: Any] = [
            "last_activity_at": Self.now(), "model": "claude-opus-5-5", "pending_decisions": 0,
            "pending_decisions_truncated": false, "provider": "Claude", "role": "project_manager",
            "session_id": seat, "status": "Running",
        ]
        return [
            "seat": seatInfo, "items": items, "has_more": more,
            "next_before": more ? "\(first):\(900 + first)" as Any : NSNull(), "operator_messages": [],
        ]
    }
}
