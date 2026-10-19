import Foundation

// The V1 answer pipeline (hub #2963), mirroring remote/web/api.js
// (`parseTargets`, `answerDecision`, `answerRefusal`, `parsePendingReceipt`) and
// the gateway in crates/rsi-remote/src/answers.rs.

let maxAnswerBytes = 2048
private let maxDecisions = 16
private let maxTargetOptions = 8

/// A positive decimal as the host echoes it: `^[1-9][0-9]*$`, kept as a string.
func isDecimalID(_ value: String) -> Bool {
    guard let first = value.first, first != "0", value.count <= 32 else { return false }
    return value.allSatisfy { $0 >= "0" && $0 <= "9" }
}

/// Canonical lowercase UUID, the only idempotency key shape the gateway takes.
func isLowerUUID(_ value: String) -> Bool {
    guard value.count == 36, let uuid = UUID(uuidString: value) else { return false }
    return uuid.uuidString.lowercased() == value
}

struct AnswerFence: Sendable, Hashable {
    let policyVersion: String
    let scopeVersion: String
}

struct PendingReceipt: Sendable, Hashable {
    static let states = ["queued", "running", "succeeded", "refused", "failed", "uncertain"]
    let key: String
    let state: String
}

enum TargetKind: String, Sendable, Hashable {
    case question
    case approval
}

/// One thing the operator may answer: a daemon decision (`pending == false`,
/// versioned and fenced) or a pending question/approval row.
struct AnswerTarget: Identifiable, Sendable, Hashable {
    let decisionKey: String
    let pending: Bool
    /// Nil for a pending target.
    let rowVersion: String?
    let digest: String
    let kind: TargetKind
    let title: String
    let detail: String
    let options: [String]
    /// Pending rows only: where an earlier answer stands.
    let receipt: PendingReceipt?

    var id: String { decisionKey }

    /// A question can always take free text; an approval needs offered options.
    var isAnswerable: Bool { kind == .question || !options.isEmpty }
}

struct AnswerTargets: Sendable, Hashable {
    let ready: Bool
    let fence: AnswerFence?
    let items: [AnswerTarget]
    let truncated: Bool
}

/// The outcome of the decision-targets read. 403/404 mean answers are off.
enum TargetsOutcome: Sendable {
    case off
    case failed(RemoteError)
    case ok(AnswerTargets)
}

// MARK: Parsing

private struct RawFence: Decodable {
    let policyVersion: String
    let scopeVersion: String

    enum CodingKeys: String, CodingKey {
        case policyVersion = "policy_version"
        case scopeVersion = "scope_version"
    }
}

private struct RawReceipt: Decodable {
    let state: String
    let receiptKey: String

    enum CodingKeys: String, CodingKey {
        case state
        case receiptKey = "receipt_key"
    }
}

private struct RawTarget: Decodable {
    let decisionKey: String
    let rowVersion: String
    let targetDigest: String
    let kind: String
    let title: String
    let detail: String
    let options: [String]

    enum CodingKeys: String, CodingKey {
        case decisionKey = "decision_key"
        case rowVersion = "row_version"
        case targetDigest = "target_digest"
        case kind, title, detail, options
    }
}

private struct RawPending: Decodable {
    let decisionId: String
    let targetDigest: String
    let kind: String
    let title: String
    let detail: String
    let options: [String]
    let receipt: RawReceipt?

    enum CodingKeys: String, CodingKey {
        case decisionId = "decision_id"
        case targetDigest = "target_digest"
        case kind, title, detail, options, receipt
    }
}

private struct RawTargets: Decodable {
    let manager: String?
    let fence: RawFence?
    let items: [RawTarget]
    let pendingItems: [RawPending]?
    let truncated: Bool?

    enum CodingKeys: String, CodingKey {
        case manager, fence, items, truncated
        case pendingItems = "pending_items"
    }
}

private func receipt(from raw: RawReceipt) throws -> PendingReceipt {
    guard PendingReceipt.states.contains(raw.state), isLowerUUID(raw.receiptKey) else {
        throw RemoteError.invalidResponse("pending receipt")
    }
    return PendingReceipt(key: raw.receiptKey, state: raw.state)
}

/// Parse a `decision-targets` body. Versions and digests stay the exact strings
/// the host sent: they are echoed back, never interpreted. `ready` needs a
/// configured manager, a live fence and at least one item.
func parseTargets(_ data: Data) throws -> AnswerTargets {
    let raw: RawTargets
    do {
        raw = try JSONDecoder().decode(RawTargets.self, from: data)
    } catch {
        throw RemoteError.invalidResponse("decision targets")
    }
    guard raw.items.count <= maxDecisions else {
        throw RemoteError.invalidResponse("decision targets")
    }
    let managerReady = raw.manager == "configured" && raw.fence != nil
    var fence: AnswerFence?
    if managerReady, let rawFence = raw.fence {
        guard isDecimalID(rawFence.policyVersion), isDecimalID(rawFence.scopeVersion) else {
            throw RemoteError.invalidResponse("decision fence")
        }
        fence = AnswerFence(policyVersion: rawFence.policyVersion, scopeVersion: rawFence.scopeVersion)
    }
    var items: [AnswerTarget] = []
    if managerReady {
        for item in raw.items {
            guard let kind = TargetKind(rawValue: item.kind), !item.decisionKey.isEmpty,
                item.decisionKey.count <= 160, isDecimalID(item.rowVersion),
                item.targetDigest.count <= 128, item.title.count <= 512,
                item.detail.count <= 2048, item.options.count <= maxTargetOptions
            else { throw RemoteError.invalidResponse("decision target") }
            items.append(
                AnswerTarget(
                    decisionKey: item.decisionKey, pending: false, rowVersion: item.rowVersion,
                    digest: item.targetDigest, kind: kind, title: item.title, detail: item.detail,
                    options: item.options, receipt: nil))
        }
    }
    let rawPending = raw.pendingItems ?? []
    guard rawPending.count <= maxDecisions else {
        throw RemoteError.invalidResponse("pending targets")
    }
    for item in rawPending {
        guard let kind = TargetKind(rawValue: item.kind),
            item.decisionId.hasPrefix(kind == .question ? "pending-question:" : "pending-approval:"),
            item.decisionId.count <= 160, item.targetDigest.count <= 128,
            item.title.count <= 512, item.detail.count <= 2048,
            item.options.count <= maxTargetOptions
        else { throw RemoteError.invalidResponse("pending target") }
        items.append(
            AnswerTarget(
                decisionKey: item.decisionId, pending: true, rowVersion: nil,
                digest: item.targetDigest, kind: kind, title: item.title, detail: item.detail,
                options: item.options, receipt: try item.receipt.map(receipt(from:))))
    }
    return AnswerTargets(
        ready: !items.isEmpty, fence: fence, items: items, truncated: raw.truncated == true)
}

// MARK: Answer errors

enum AnswerErrorKind: String, Sendable, Hashable {
    /// 409: the decision changed; discard the draft and refresh.
    case stale
    /// 403: the gateway refused the answer.
    case denied
    /// 429 with `Retry-After`.
    case rateLimited
    /// 503 with `Retry-After`.
    case busy
    /// 502/504 `outcome_unknown` or a lost connection after sending: it is not
    /// known whether the answer was delivered. Retry only under the same key.
    case unknown
    /// The host could not be asked; nothing was sent.
    case unavailable
    /// A bad answer, refused before or by the gateway (400).
    case invalid
    /// 401 after one sign-in retry, or sign-in failed.
    case session
    case unexpected
}

struct AnswerError: Error, Sendable, Hashable {
    let kind: AnswerErrorKind
    let message: String
    let status: Int
    let retryAfter: Int?

    init(_ kind: AnswerErrorKind, _ message: String, status: Int = 0, retryAfter: Int? = nil) {
        self.kind = kind
        self.message = message
        self.status = status
        self.retryAfter = retryAfter
    }
}

/// `Retry-After` in whole seconds, 1...3600, else nil.
func retryAfterSeconds(_ header: String?) -> Int? {
    guard let header, let seconds = Int(header.trimmingCharacters(in: .whitespaces)), seconds >= 1
    else { return nil }
    return min(seconds, 3600)
}

private func waitText(_ seconds: Int?) -> String {
    guard let seconds else { return "a moment" }
    return "\(seconds) second\(seconds == 1 ? "" : "s")"
}

/// Map an answer POST status (after the one 401 retry) like the web client's
/// `answerRefusal`.
func answerRefusal(status: Int, code: String, retryAfter: Int?) -> AnswerError {
    switch status {
    case 409:
        return AnswerError(
            .stale,
            "This decision changed. Refresh to see its current state; your draft was discarded.",
            status: 409)
    case 403:
        return AnswerError(
            .denied,
            "The gateway refused this answer. Answers may be turned off, or the host declined it. Nothing was changed.",
            status: 403)
    case 429:
        return AnswerError(
            .rateLimited,
            "Too many answers too quickly. Nothing was sent. Try again in \(waitText(retryAfter)).",
            status: 429, retryAfter: retryAfter)
    case 503:
        return AnswerError(
            .busy,
            "The RSI host is busy. Nothing was sent. Try again in \(waitText(retryAfter)).",
            status: 503, retryAfter: retryAfter)
    case 502 where code == "outcome_unknown":
        return AnswerError(
            .unknown,
            "The connection to the RSI host failed after sending, so it is not known whether your answer was delivered.",
            status: 502)
    case 504 where code == "outcome_unknown":
        return AnswerError(
            .unknown,
            "The RSI host did not reply in time, so it is not known whether your answer was delivered.",
            status: 504)
    case 504:
        return AnswerError(
            .unavailable,
            "The RSI host did not reply in time while checking this decision. Nothing was sent. Try again.",
            status: 504)
    case 502:
        return AnswerError(
            .unavailable,
            "The RSI daemon is unavailable. Nothing was sent. Check that rsid is running on the host.",
            status: 502)
    case 400:
        return AnswerError(
            .invalid, "The gateway could not accept this answer. Check its length and try again.",
            status: 400)
    case 401:
        return AnswerError(
            .session, "Your session has expired. Pull to refresh to sign in again. Nothing was sent.",
            status: 401)
    default:
        return AnswerError(
            .unexpected,
            "The RSI host returned \(status). Refresh to check whether the decision is still waiting.",
            status: status)
    }
}

// MARK: Request and receipt

/// The first problem with an answer text, or nil when it can be sent.
func answerProblem(_ text: String) -> String? {
    if text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
        return "Write or choose an answer first."
    }
    if text.contains("\0") { return "The answer cannot contain a NUL character." }
    if text.utf8.count > maxAnswerBytes { return "The answer is longer than \(maxAnswerBytes) bytes." }
    return nil
}

/// One frozen answer. A retry after an unknown outcome resends exactly this
/// value, fence and idempotency key included.
struct AnswerRequest: Sendable, Hashable {
    let decisionKey: String
    let pending: Bool
    let rowVersion: String?
    let digest: String
    let fence: AnswerFence?
    let answer: String
    let idempotencyKey: String

    /// Nothing is sent when this throws.
    func validate() throws {
        guard answerProblem(answer) == nil, isLowerUUID(idempotencyKey), !decisionKey.isEmpty
        else { throw AnswerError(.invalid, "This answer is not valid. Nothing was sent.") }
        if !pending {
            guard let rowVersion, isDecimalID(rowVersion), let fence,
                isDecimalID(fence.policyVersion), isDecimalID(fence.scopeVersion)
            else { throw AnswerError(.invalid, "This answer is not valid. Nothing was sent.") }
        }
    }

    /// The JSON body: `answer` for a daemon decision, `answer-pending` for a
    /// pending row. Keys are sorted so the bytes are stable across retries.
    func body() throws -> Data {
        try validate()
        let object: [String: Any]
        if pending {
            object = [
                "decision_id": decisionKey,
                "expected_target_digest": digest,
                "answer": answer,
                "idempotency_key": idempotencyKey,
            ]
        } else {
            object = [
                "decision_key": decisionKey,
                "expected_row_version": rowVersion ?? "",
                "target_digest": digest,
                "fence": [
                    "policy_version": fence?.policyVersion ?? "",
                    "scope_version": fence?.scopeVersion ?? "",
                ],
                "answer": answer,
                "idempotency_key": idempotencyKey,
            ]
        }
        return try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }

    var route: String { pending ? "answer-pending" : "answer" }
}

enum AnswerReceipt: Sendable, Hashable {
    /// An `answer` accepted: the receipt key and whether it was a replay.
    case delivered(key: String, deduplicated: Bool)
    /// An `answer-pending` accepted: where the row's delivery stands.
    case pending(PendingReceipt)
}

/// Read a 2xx answer body. A daemon answer without a readable body is still
/// delivered; a pending answer without a valid receipt is not known.
func parseAnswerReceipt(_ data: Data, pending: Bool) throws -> AnswerReceipt {
    if pending {
        guard let raw = try? JSONDecoder().decode(RawReceipt.self, from: data),
            let parsed = try? receipt(from: raw)
        else {
            throw AnswerError(
                .unknown, "The answer receipt could not be read. Retry the same answer.")
        }
        return .pending(parsed)
    }
    let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    let key = (object?["key"] as? String).flatMap { $0.count <= 256 ? $0 : nil } ?? ""
    return .delivered(key: key, deduplicated: object?["deduplicated"] as? Bool == true)
}

/// What the operator sees for a pending row's receipt. It never claims the
/// decision is answered; the next read shows what happened.
func describePendingReceipt(_ receipt: PendingReceipt) -> (message: String, tone: AnswerTone) {
    switch receipt.state {
    case "uncertain":
        return (
            "The answer may have been consumed. The host will not send it again. Inspect the session on your RSI host. Receipt \(receipt.key).",
            .warn
        )
    case "succeeded":
        return (
            "The host established delivery. Native approval consumption still waits for provider confirmation. Receipt \(receipt.key).",
            .ok
        )
    case "refused", "failed":
        return (
            "The host refused or failed this answer. Refresh the session on your RSI host. Receipt \(receipt.key).",
            .error
        )
    default:
        return (
            "The host recorded the answer and is waiting to deliver it. Receipt \(receipt.key).", .ok
        )
    }
}

enum AnswerTone: Sendable, Hashable {
    case idle, busy, ok, warn, error
}
