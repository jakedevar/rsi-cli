import Foundation

// Read models for the RSI Remote gateway's V1 result bodies
// (docs/rsi-remote/contracts-v1.md). The gateway returns each method's
// `result` object as the HTTP body. These types decode what the app shows and
// ignore fields it does not use; the strict byte-boundary codec stays on the
// host (Rust) and in the TypeScript package. Every V1 fixture response for the
// methods below must decode (RSIRemoteTests/FixtureDecodingTests).

/// A V1 source enum: `{state:"known",value}` or `{state:"unknown",label}`.
/// Unknown values keep their label and never acquire a known meaning.
enum Tagged: Decodable, Sendable, Hashable {
    case known(String)
    case unknown(String)

    private enum CodingKeys: String, CodingKey { case state, value, label }

    init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(String.self, forKey: .state) {
        case "known": self = .known(try container.decode(String.self, forKey: .value))
        case "unknown": self = .unknown(try container.decode(String.self, forKey: .label))
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .state, in: container, debugDescription: "tagged state \(other)")
        }
    }

    /// The known value, or nil for an unknown label.
    var knownValue: String? {
        if case .known(let value) = self { value } else { nil }
    }

    /// Positive display text: the value, or `Unknown: <label>` (the contract's hint).
    var display: String {
        switch self {
        case .known(let value): value
        case .unknown(let label): "Unknown: \(label)"
        }
    }
}

/// `{kind, token}` continuation for the same route, or null on the last page.
struct WireCursor: Decodable, Sendable, Hashable {
    let kind: String
    let token: String
}

struct WireProject: Decodable, Sendable, Hashable, Identifiable {
    let id: String
    let name: String
}

/// `RemoteListProjectsV1` (`GET /api/v1/projects`).
struct ProjectListResult: Decodable, Sendable {
    let version: String
    let observedAt: String
    let nextCursor: WireCursor?
    let complete: Bool
    let degraded: [String]
    let items: [WireProject]

    enum CodingKeys: String, CodingKey {
        case version, complete, degraded, items
        case observedAt = "observed_at"
        case nextCursor = "next_cursor"
    }
}

struct SessionAttention: Decodable, Sendable, Hashable {
    let requiresLocalAction: Bool
    let incomplete: Bool

    enum CodingKeys: String, CodingKey {
        case requiresLocalAction = "requires_local_action"
        case incomplete
    }
}

struct SessionSummary: Decodable, Sendable, Hashable, Identifiable {
    let id: String
    let projectId: String
    let parentId: String?
    let continuedFrom: String?
    let kind: Tagged
    let ownTitle: String
    let provider: Tagged
    let status: Tagged
    let updatedAt: String
    let attention: SessionAttention

    enum CodingKeys: String, CodingKey {
        case id, kind, provider, status, attention
        case projectId = "project_id"
        case parentId = "parent_id"
        case continuedFrom = "continued_from"
        case ownTitle = "own_title"
        case updatedAt = "updated_at"
    }

    /// The session waits on the operator (a question or an approval).
    var needsYou: Bool { attention.requiresLocalAction }
}

/// `RemoteListSessionsV1` (`GET /api/v1/projects/{p}/sessions`).
struct SessionListResult: Decodable, Sendable {
    let nextCursor: WireCursor?
    let complete: Bool
    let degraded: [String]
    let project: WireProject
    let items: [SessionSummary]

    enum CodingKeys: String, CodingKey {
        case complete, degraded, project, items
        case nextCursor = "next_cursor"
    }
}

/// A bounded preview field: text plus how complete it is.
struct DisplayField: Decodable, Sendable, Hashable {
    let text: String
    let state: String

    /// The operator-facing text: truncated previews are marked, unavailable
    /// fields show an inert placeholder instead of their (empty) text.
    var shown: String {
        switch state {
        case "unavailable": "Unavailable"
        case "truncated": "\(text) …"
        default: text
        }
    }
}

/// `RemoteGetSessionV1` (`GET /api/v1/projects/{p}/sessions/{s}`).
struct SessionDetailResult: Decodable, Sendable {
    struct Item: Decodable, Sendable {
        let summary: SessionSummary
        let ownTitle: String
        let query: DisplayField
        let model: DisplayField

        enum CodingKeys: String, CodingKey {
            case summary, query, model
            case ownTitle = "own_title"
        }
    }

    let complete: Bool
    let item: Item
}

struct HistoryEvent: Decodable, Sendable, Hashable, Identifiable {
    /// Positive i64 as a decimal string.
    let id: String
    let sequence: Int32
    let kind: Tagged
    let role: Tagged?
    let createdAt: String
    let text: String
    let truncated: Bool
    let toolName: String?
    /// The provider call id a ToolUse and its ToolResult share (hub #3797 and
    /// the V1 `tool_pair_key`). Nil when the hub has none for the event (no id
    /// stored, or one over 256 bytes) or predates it.
    let toolPairKey: String?
    /// `exact`, `ambiguous`, `oversized` or `missing`; kept as text so a new
    /// state is carried, not rejected. Nil from a hub that predates it.
    let pairingState: String?

    enum CodingKeys: String, CodingKey {
        case id, sequence, kind, role, text, truncated
        case createdAt = "created_at"
        case toolName = "tool_name"
        case toolPairKey = "tool_pair_key"
        case pairingState = "pairing_state"
    }

    enum Speaker: Sendable { case user, assistant, tool, system, thinking }

    /// The same mapping the web client uses (remote/web/api.js `eventKind`).
    var speaker: Speaker {
        switch kind.knownValue {
        case "Message":
            switch role?.knownValue {
            case "User": .user
            case "Assistant": .assistant
            default: .system
            }
        case "ToolUse", "ToolResult": .tool
        case "Thinking": .thinking
        default: .system
        }
    }
}

/// `RemoteGetHistoryPageV1` (`GET .../sessions/{s}/history`).
struct HistoryPageResult: Decodable, Sendable {
    let complete: Bool
    let nextCursor: WireCursor?
    let items: [HistoryEvent]

    enum CodingKeys: String, CodingKey {
        case complete, items
        case nextCursor = "next_cursor"
    }
}

struct DecisionOption: Decodable, Sendable, Hashable {
    let label: String
    let description: String?
}

struct DecisionQuestion: Decodable, Sendable, Hashable {
    let header: String?
    let question: String
    let options: [DecisionOption]
    let multiSelect: Bool

    enum CodingKeys: String, CodingKey {
        case header, question, options
        case multiSelect = "multi_select"
    }
}

/// `generic_questions`, `native_approval{method,description}` or
/// `legacy_approval{tool_name}`.
struct DecisionDisplay: Decodable, Sendable, Hashable {
    let kind: String
    let method: DisplayField?
    let description: DisplayField?
    let toolName: DisplayField?

    enum CodingKeys: String, CodingKey {
        case kind, method, description
        case toolName = "tool_name"
    }
}

struct Decision: Decodable, Sendable, Hashable, Identifiable {
    let id: String
    let kind: String
    let closureState: String
    let display: DecisionDisplay
    let questions: [DecisionQuestion]
    let requiresLocalAction: Bool

    enum CodingKeys: String, CodingKey {
        case id, kind, display, questions
        case closureState = "closure_state"
        case requiresLocalAction = "requires_local_action"
    }

    var isOpen: Bool { closureState != "closed" }

    /// One-line title, as the web client shows it.
    var title: String {
        switch kind {
        case "native_approval": display.method?.shown ?? "Approval"
        case "legacy_approval": display.toolName?.shown ?? "Approval"
        default: questions.first?.question ?? "A question is waiting"
        }
    }

    var detail: String {
        kind == "native_approval" ? (display.description?.shown ?? "") : ""
    }
}

/// `RemoteGetDecisionsV1` (`GET .../sessions/{s}/decisions`).
struct DecisionListResult: Decodable, Sendable {
    let complete: Bool
    let projectId: String
    let sessionId: String
    let items: [Decision]

    enum CodingKeys: String, CodingKey {
        case complete, items
        case projectId = "project_id"
        case sessionId = "session_id"
    }
}

/// `RemoteGetInfoV1` (`GET /api/v1/info`).
struct InfoResult: Decodable, Sendable {
    struct Item: Decodable, Sendable {
        let `protocol`: String
        let daemonBootId: String
        let requiredCapabilities: [String]

        enum CodingKeys: String, CodingKey {
            case `protocol`
            case daemonBootId = "daemon_boot_id"
            case requiredCapabilities = "required_capabilities"
        }
    }

    let version: String
    let item: Item
}

/// A V1 safe error: a closed code, correlation id and bounded retry advice.
struct SafeErrorBody: Decodable, Sendable {
    struct Retry: Decodable, Sendable {
        let action: String
        let afterMs: Int?

        enum CodingKeys: String, CodingKey {
            case action
            case afterMs = "after_ms"
        }
    }

    let code: String
    let correlationId: String?
    let retry: Retry?

    enum CodingKeys: String, CodingKey {
        case code, retry
        case correlationId = "correlation_id"
    }
}
