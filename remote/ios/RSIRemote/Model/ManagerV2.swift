import Foundation

// Gateway API v2: manager seats, their conversations and one write, a message
// to a seat (docs/rsi-remote/contracts-v2.md, wire types
// `rsi_common::remote_manager_v2`). Bodies are snake_case. Every fixture under
// remote/contracts/v2/fixtures decodes or maps here (RSIRemoteTests/V2FixtureTests).

/// The contract versions this build speaks, shown in Settings.
let supportedContracts = ["v1", "v2"]

// MARK: Seats

enum SeatRole: String, Sendable, Hashable, CaseIterable {
    case projectManager = "project_manager"
    case globalManager = "global_manager"

    /// The toggle label.
    var label: String {
        switch self {
        case .projectManager: "PM"
        case .globalManager: "Global"
        }
    }
}

/// `ManagerSeatV2`: one manager seat, named by its current session id.
struct ManagerSeat: Decodable, Sendable, Hashable {
    let sessionId: String
    /// `project_manager` or `global_manager`; kept as text so a new role is shown, not rejected.
    let role: String
    let status: String
    let provider: String
    let model: String?
    let lastActivityAt: String
    /// Null for the global seat: it has no per-project decision ledger.
    let pendingDecisions: Int?
    let pendingDecisionsTruncated: Bool

    enum CodingKeys: String, CodingKey {
        case sessionId = "session_id"
        case role, status, provider, model
        case lastActivityAt = "last_activity_at"
        case pendingDecisions = "pending_decisions"
        case pendingDecisionsTruncated = "pending_decisions_truncated"
    }

    /// `2`, or `25+` when the daemon's capped scan stopped; nil when the
    /// contract gives no count (global seat) or there is nothing waiting.
    var pendingBadge: String? {
        guard let pendingDecisions, pendingDecisions > 0 || pendingDecisionsTruncated else { return nil }
        return pendingDecisionsTruncated ? "\(pendingDecisions)+" : "\(pendingDecisions)"
    }

    /// `Running · Claude claude-opus-5-5`
    var summary: String {
        [status, [provider, model ?? ""].filter { !$0.isEmpty }.joined(separator: " ")]
            .filter { !$0.isEmpty }.joined(separator: " · ")
    }
}

/// `ProjectSeatsV2`: one exposed project with its PM seat and the covering global seat.
struct ProjectSeats: Decodable, Sendable, Hashable {
    let projectId: String
    let name: String
    let projectManager: ManagerSeat?
    let globalManager: ManagerSeat?

    enum CodingKeys: String, CodingKey {
        case projectId = "project_id"
        case name
        case projectManager = "project_manager"
        case globalManager = "global_manager"
    }

    func seat(_ role: SeatRole) -> ManagerSeat? {
        switch role {
        case .projectManager: projectManager
        case .globalManager: globalManager
        }
    }

    /// Whether `session` is still one of this project's manager seats.
    func lists(_ session: String) -> Bool {
        projectManager?.sessionId == session || globalManager?.sessionId == session
    }
}

/// `GET /api/v2/projects`.
struct ProjectSeatsList: Decodable, Sendable, Hashable {
    let projects: [ProjectSeats]
    /// hub #2988: the policy's `allow_messages`, so the phone knows before a
    /// send whether messaging is on. Nil from a gateway that predates it; the
    /// app then falls back to reading a bare 403 after a send.
    let messagesEnabled: Bool?

    enum CodingKeys: String, CodingKey {
        case projects
        case messagesEnabled = "messages_enabled"
    }

    init(projects: [ProjectSeats], messagesEnabled: Bool? = nil) {
        self.projects = projects
        self.messagesEnabled = messagesEnabled
    }
}

// MARK: Conversation

/// `ConversationItemV2`. `speaker` is `operator`, `manager` or `system`.
struct ConversationItem: Decodable, Sendable, Hashable, Identifiable {
    let id: String
    let sequence: Int
    let speaker: String
    let text: String
    let truncated: Bool
    let createdAt: String
    let operatorMessageId: String?

    enum CodingKeys: String, CodingKey {
        case id, sequence, speaker, text, truncated
        case createdAt = "created_at"
        case operatorMessageId = "operator_message_id"
    }

    var speakerLabel: String {
        switch speaker {
        case "operator": "You"
        case "manager": "Manager"
        case "system": "System"
        default: speaker
        }
    }

    /// Newest first: by sequence, then id, both descending.
    static func newerFirst(_ a: ConversationItem, _ b: ConversationItem) -> Bool {
        if a.sequence != b.sequence { return a.sequence > b.sequence }
        return (Int64(a.id) ?? 0) > (Int64(b.id) ?? 0)
    }
}

/// `OperatorMessageStateV2`: the delivery view of a recent operator message.
struct OperatorMessageState: Decodable, Sendable, Hashable, Identifiable {
    let messageId: String
    let state: String
    let createdAt: String
    let updatedAt: String
    let deliveredAt: String?
    let textLen: Int

    var id: String { messageId }

    enum CodingKeys: String, CodingKey {
        case messageId = "message_id"
        case state
        case createdAt = "created_at"
        case updatedAt = "updated_at"
        case deliveredAt = "delivered_at"
        case textLen = "text_len"
    }
}

/// `GET …/conversation`: 25 message items per page, newest first.
struct ConversationPage: Decodable, Sendable, Hashable {
    let seat: ManagerSeat
    let items: [ConversationItem]
    /// Pass back unchanged as `?before=`; null on the oldest page.
    let nextBefore: String?
    let hasMore: Bool
    /// First page only: up to 10 newest operator messages, never their text.
    let operatorMessages: [OperatorMessageState]

    enum CodingKeys: String, CodingKey {
        case seat, items
        case nextBefore = "next_before"
        case hasMore = "has_more"
        case operatorMessages = "operator_messages"
    }
}

/// The gateway's `before` grammar, `<sequence>:<id>` (sequence may be
/// negative, id a positive canonical integer). The client only hands back what
/// the gateway gave it; anything else is not sent.
func isConversationCursor(_ value: String) -> Bool {
    let parts = value.split(separator: ":", omittingEmptySubsequences: false)
    guard parts.count == 2 else { return false }
    let sequence = parts[0].hasPrefix("-") ? parts[0].dropFirst() : parts[0]
    guard !sequence.isEmpty, sequence.allSatisfy(\.isASCII), sequence.allSatisfy(\.isNumber),
        Int32(parts[0]) != nil
    else { return false }
    guard let id = Int64(parts[1]), id > 0, String(id) == parts[1] else { return false }
    return true
}

// MARK: Delivery

/// What a message `state` (send receipt or `operator_messages`) means to the operator.
func describeDelivery(_ state: String) -> (text: String, tone: AnswerTone) {
    switch state {
    case "queued":
        ("Queued. The manager gets it at its next turn.", .busy)
    case "dispatching", "effect_possible":
        ("Being handed to the manager now.", .busy)
    case "delivered":
        ("Delivered to the manager.", .ok)
    case "uncertain":
        ("The host restarted while delivering this. It will not be sent again; check the conversation.", .warn)
    case "withdrawn":
        ("Withdrawn on the hub before delivery.", .warn)
    default:
        ("State: \(state).", .idle)
    }
}

// MARK: Send

let maxMessageBytes = 4096

/// The first problem with a message text, or nil when it can be sent
/// (`valid_message_text`: 1–4096 bytes, not blank, no NUL).
func messageProblem(_ text: String) -> String? {
    if text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return "Write a message first." }
    if text.contains("\0") { return "The message cannot contain a NUL character." }
    if text.utf8.count > maxMessageBytes {
        return "The message is longer than \(maxMessageBytes) bytes."
    }
    return nil
}

/// One frozen send. A retry after an unknown outcome resends exactly this
/// value: same seat, same text, same idempotency key.
struct MessageRequest: Sendable, Hashable {
    let project: String
    let seat: String
    let text: String
    let idempotencyKey: String

    /// The body: exactly `text` and `idempotency_key`, sorted so retries are byte-identical.
    func body() throws -> Data {
        guard messageProblem(text) == nil, isLowerUUID(idempotencyKey) else {
            throw MessageError(.invalid, "This message is not valid. Nothing was sent.")
        }
        return try JSONSerialization.data(
            withJSONObject: ["text": text, "idempotency_key": idempotencyKey], options: [.sortedKeys])
    }

    var path: String { managerPath(project, seat) + "/messages" }
}

func managerPath(_ project: String, _ seat: String) -> String {
    "/api/v2/projects/\(escape(project))/managers/\(escape(seat))"
}

/// `SendMessageReceiptV2`.
struct MessageReceipt: Decodable, Sendable, Hashable {
    let messageId: String
    let seatSessionId: String
    let state: String
    let createdAt: String
    let deduplicated: Bool

    enum CodingKeys: String, CodingKey {
        case messageId = "message_id"
        case seatSessionId = "seat_session_id"
        case state
        case createdAt = "created_at"
        case deduplicated
    }
}

enum MessageErrorKind: String, Sendable, Hashable {
    /// 400: the gateway refused the body or text; nothing was sent.
    case invalid
    /// A bare 403: bad Origin/CSRF, messaging off, project not exposed or not a
    /// seat. The contract gives no detail; `ManagerChat` re-lists the seats to
    /// tell `messagingOff` from `seatChanged`.
    case forbidden
    /// The gateway refused while the seat is still listed: the operator's
    /// `allow_messages` switch is off (the only remaining contract reason for a
    /// client that sends the right Origin and CSRF).
    case messagingOff
    /// 403/404 for a seat that is no longer listed, or `409 seat_not_live`.
    case seatChanged
    /// `409 idempotency_conflict`: this key was used for other text.
    case conflict
    /// 429 with `Retry-After`; nothing was sent.
    case rateLimited
    /// 503 `busy`/`audit_unavailable` with `Retry-After`; nothing was sent.
    case busy
    /// 502/504 `outcome_unknown` or a lost connection after sending. Retry
    /// only under the same key.
    case unknown
    /// The host could not be asked (bare 502/504, unreachable); nothing was sent.
    case unavailable
    /// 401 after one sign-in retry, or sign-in failed; nothing was sent.
    case session
    case unexpected
}

struct MessageError: Error, Sendable, Hashable {
    let kind: MessageErrorKind
    let message: String
    let status: Int
    let retryAfter: Int?

    init(_ kind: MessageErrorKind, _ message: String, status: Int = 0, retryAfter: Int? = nil) {
        self.kind = kind
        self.message = message
        self.status = status
        self.retryAfter = retryAfter
    }

    /// The request may have queued a message: the key must stay frozen.
    var mayHaveSent: Bool { kind == .unknown }

    static let messagingOffText =
        "Messaging from the phone is off. Turn it on in the hub's Remote settings (Settings → Integrations → Remote → Remote access, m), then send again."

    static func messagingOff() -> MessageError {
        MessageError(.messagingOff, messagingOffText, status: 403)
    }

    static func seatChanged(status: Int) -> MessageError {
        MessageError(
            .seatChanged,
            "This manager seat changed (rotation or succession). The seats were reloaded; nothing was sent.",
            status: status)
    }
}

private func waitText(_ seconds: Int?) -> String {
    guard let seconds else { return "a moment" }
    return "\(seconds) second\(seconds == 1 ? "" : "s")"
}

/// Map a message POST status (after the one 401 retry) to its outcome.
func messageRefusal(status: Int, code: String, retryAfter: Int?) -> MessageError {
    switch status {
    case 400:
        return MessageError(
            .invalid, "The gateway could not accept this message. Check its length; nothing was sent.",
            status: 400)
    case 401:
        return MessageError(
            .session, "Your session has expired and signing in again failed. Nothing was sent.",
            status: 401)
    case 403:
        return MessageError(
            .forbidden,
            "The gateway refused this message. Messaging may be off in the hub's Remote settings, or the seat changed. Nothing was sent.",
            status: 403)
    case 404:
        return .seatChanged(status: 404)
    case 409 where code == "seat_not_live":
        return .seatChanged(status: 409)
    case 409:
        return MessageError(
            .conflict,
            "This send key was already used for different text, so nothing new was queued. Send again to use a new key.",
            status: 409)
    case 429:
        return MessageError(
            .rateLimited,
            "Too many messages too quickly. Nothing was sent. Try again in \(waitText(retryAfter)).",
            status: 429, retryAfter: retryAfter)
    case 503:
        return MessageError(
            .busy, "The RSI host is busy. Nothing was sent. Try again in \(waitText(retryAfter)).",
            status: 503, retryAfter: retryAfter)
    case 502 where code == "outcome_unknown", 504 where code == "outcome_unknown":
        return MessageError(
            .unknown,
            "Not known whether delivered: the RSI host did not confirm the send. Retry sends the same message under the same key, so it cannot arrive twice.",
            status: status)
    case 502, 504:
        return MessageError(
            .unavailable,
            "The RSI daemon did not answer while checking the seat. Nothing was sent. Try again.",
            status: status)
    default:
        return MessageError(
            .unexpected, "The RSI host returned \(status). Check the conversation before sending again.",
            status: status)
    }
}

// MARK: Draft

/// The compose box for one seat, with the slice-3 key rules: the same text to
/// the same seat reuses its key; after an unknown outcome the attempt is frozen
/// (text locked) until a definitive reply or the operator discards it; a new
/// message gets a new key.
struct MessageDraft: Sendable, Hashable {
    enum Phase: Sendable, Hashable {
        case idle
        case sending
        case sent(MessageReceipt)
        case failed(MessageError)
    }

    var text = ""
    private(set) var attempt: MessageRequest?
    private(set) var uncertain = false
    private(set) var phase = Phase.idle

    /// After an unknown outcome the text cannot change: only Retry or Discard.
    var locked: Bool { uncertain }
    var sending: Bool { phase == .sending }

    /// The request to send now. Throws (nothing sent) for an invalid text.
    mutating func begin(project: String, seat: String, newKey: () -> String) throws -> MessageRequest {
        if uncertain, let attempt {
            phase = .sending
            return attempt
        }
        if let problem = messageProblem(text) { throw MessageError(.invalid, problem) }
        let request: MessageRequest
        if let attempt, attempt.text == text, attempt.seat == seat, attempt.project == project {
            request = attempt
        } else {
            request = MessageRequest(project: project, seat: seat, text: text, idempotencyKey: newKey())
        }
        attempt = request
        phase = .sending
        return request
    }

    mutating func finish(_ result: Result<MessageReceipt, MessageError>) {
        switch result {
        case .success(let receipt):
            text = ""
            attempt = nil
            uncertain = false
            phase = .sent(receipt)
        case .failure(let error):
            if error.mayHaveSent {
                uncertain = true
            } else if error.kind == .conflict {
                // Definitive: this key will never queue this text. Next send gets a new key.
                attempt = nil
                uncertain = false
            } else if error.kind == .seatChanged, !uncertain {
                attempt = nil
            }
            phase = .failed(error)
        }
    }

    /// Give up on an unknown attempt (the operator checks the conversation).
    mutating func discard() {
        text = ""
        attempt = nil
        uncertain = false
        phase = .idle
    }
}

// MARK: Polling

/// Poll schedule for a visible conversation: 10 s, doubling after each
/// consecutive error to at most 120 s, back to 10 s on success. A
/// `Retry-After` longer than the schedule wins.
struct PollBackoff: Sendable, Hashable {
    static let base: Duration = .seconds(10)
    static let maximum: Duration = .seconds(120)

    private(set) var failures = 0
    private var retryAfter: Int?

    var interval: Duration {
        var seconds = 10
        for _ in 0..<min(failures, 8) { seconds = min(seconds * 2, 120) }
        if let retryAfter { seconds = max(seconds, min(retryAfter, 3600)) }
        return .seconds(seconds)
    }

    mutating func succeeded() {
        failures = 0
        retryAfter = nil
    }

    mutating func failed(retryAfter: Int? = nil) {
        failures += 1
        self.retryAfter = retryAfter
    }
}
