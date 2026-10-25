import Foundation

// The conversation as a chat app shows it (#502): turns by role, tool calls
// folded away until asked for, a tail that follows new output. Everything
// here is pure so the tests can drive it without a view.

/// Who an event is from, in the one vocabulary both conversation sources map to.
enum ChatRole: Sendable, Hashable {
    case user, assistant, tool, thinking, system
}

/// What a tool event is: the call, or what came back.
enum ToolPhase: Sendable, Hashable {
    case use, result
}

/// One event of a conversation, whichever endpoint it came from.
struct ChatEvent: Sendable, Hashable, Identifiable {
    let id: String
    let sequence: Int
    let role: ChatRole
    let text: String
    let createdAt: String
    let truncated: Bool
    /// Tool events only.
    let toolName: String?
    let phase: ToolPhase?
    /// The hub's id of the operator message this item is (manager chat only).
    let operatorMessageId: String?
    /// Tool events only: the provider call id a use and its result share, when
    /// the hub served one (hub #3797). Nil pairs oldest-first, as before.
    let callId: String?
    /// The key the phone sent with this operator message (manager chat only).
    let idempotencyKey: String?

    init(
        id: String, sequence: Int, role: ChatRole, text: String, createdAt: String,
        truncated: Bool = false, toolName: String? = nil, phase: ToolPhase? = nil,
        operatorMessageId: String? = nil, callId: String? = nil, idempotencyKey: String? = nil
    ) {
        self.id = id
        self.sequence = sequence
        self.role = role
        self.text = text
        self.createdAt = createdAt
        self.truncated = truncated
        self.toolName = toolName
        self.phase = phase
        self.operatorMessageId = operatorMessageId
        self.callId = callId.flatMap { $0.isEmpty ? nil : $0 }
        self.idempotencyKey = idempotencyKey
    }

    /// A manager-chat item: the operator is the user, the manager the assistant,
    /// and a tool item (hub #3797) a tool event, folded like the session view's.
    init(_ item: ConversationItem) {
        if let phase = item.toolPhase {
            self.init(
                id: item.id, sequence: item.sequence, role: .tool, text: item.text,
                createdAt: item.createdAt, truncated: item.truncated, toolName: item.toolName,
                phase: phase, callId: item.toolUseId)
            return
        }
        self.init(
            id: item.id, sequence: item.sequence,
            role: { switch item.speaker {
            case "operator": .user
            case "manager": .assistant
            default: .system
            } }(),
            text: item.text, createdAt: item.createdAt, truncated: item.truncated,
            operatorMessageId: item.operatorMessageId, idempotencyKey: item.idempotencyKey)
    }

    /// A session-history event. An event kind this build does not know is
    /// shown as a system note rather than dropped.
    init(_ event: HistoryEvent) {
        let phase: ToolPhase? = {
            switch event.kind.knownValue {
            case "ToolUse": .use
            case "ToolResult": .result
            default: nil
            }
        }()
        let role: ChatRole = {
            switch event.speaker {
            case .user: .user
            case .assistant: .assistant
            case .tool: .tool
            case .thinking: .thinking
            case .system: .system
            }
        }()
        self.init(
            id: event.id, sequence: Int(event.sequence), role: role, text: event.text,
            createdAt: event.createdAt, truncated: event.truncated, toolName: event.toolName,
            phase: phase, callId: phase == nil ? nil : event.toolPairKey)
    }

    /// Oldest first: sequence, then the numeric id (ids are decimal strings).
    static func olderFirst(_ a: ChatEvent, _ b: ChatEvent) -> Bool {
        if a.sequence != b.sequence { return a.sequence < b.sequence }
        let (x, y) = (Int64(a.id), Int64(b.id))
        if let x, let y, x != y { return x < y }
        return a.id < b.id
    }
}

/// One tool call: what was asked and what came back. Either half can be
/// missing (the page began between them, or the call is still running).
struct ToolCall: Sendable, Hashable, Identifiable {
    var use: ChatEvent?
    var result: ChatEvent?

    var id: String { (use ?? result)?.id ?? "" }
    var name: String { use?.toolName ?? result?.toolName ?? "tool" }
    /// The provider call id both halves share, when the hub served one.
    var callId: String? { use?.callId ?? result?.callId }
    /// Still waiting for its result.
    var running: Bool { use != nil && result == nil }
    var createdAt: String { (use ?? result)?.createdAt ?? "" }
}

/// A piece of a turn.
enum TurnPart: Sendable, Hashable, Identifiable {
    case text(ChatEvent)
    case thinking(ChatEvent)
    /// A run of consecutive tool calls, shown as one folded row.
    case tools(id: String, calls: [ToolCall])

    var id: String {
        switch self {
        case .text(let event): "text:" + event.id
        case .thinking(let event): "thinking:" + event.id
        case .tools(let id, _): "tools:" + id
        }
    }
}

enum TurnRole: Sendable, Hashable {
    case user, assistant, system
}

/// A run of one speaker's output: the operator's messages, everything the
/// assistant did until the operator spoke again, or system notes.
struct Turn: Sendable, Hashable, Identifiable {
    /// The first event's id: stable while newer events append.
    let id: String
    let role: TurnRole
    var parts: [TurnPart]
    let createdAt: String
    /// Events in this turn (a tool call counts its use and its result).
    var eventCount: Int

    var calls: [ToolCall] {
        parts.flatMap { part -> [ToolCall] in
            if case .tools(_, let calls) = part { calls } else { [] }
        }
    }
}

enum ChatTranscript {
    /// Group events into turns. Input order does not matter (the hub pages
    /// newest first); duplicates by id collapse. A tool or thinking event with
    /// no assistant turn open starts one: that is the assistant working.
    static func build(_ events: [ChatEvent]) -> [Turn] {
        var seen = Set<String>()
        let ordered = events.sorted(by: ChatEvent.olderFirst).filter { seen.insert($0.id).inserted }
        var turns: [Turn] = []
        for event in ordered {
            let role: TurnRole = {
                switch event.role {
                case .user: .user
                case .system: .system
                case .assistant, .tool, .thinking: .assistant
                }
            }()
            if turns.last?.role != role {
                turns.append(Turn(id: event.id, role: role, parts: [], createdAt: event.createdAt, eventCount: 0))
            }
            let last = turns.count - 1
            turns[last].eventCount += 1
            switch event.role {
            case .user, .system, .assistant:
                turns[last].parts.append(.text(event))
            case .thinking:
                turns[last].parts.append(.thinking(event))
            case .tool:
                turns[last].addTool(event)
            }
        }
        return turns
    }

    static func build(items: [ConversationItem]) -> [Turn] { build(items.map(ChatEvent.init)) }
    static func build(history: [HistoryEvent]) -> [Turn] { build(history.map(ChatEvent.init)) }

    /// The id and count of the newest event across `turns`, for the tail follower.
    static func newest(_ turns: [Turn]) -> (id: String?, count: Int) {
        (turns.last.flatMap(lastEventID), turns.reduce(0) { $0 + $1.eventCount })
    }

    private static func lastEventID(_ turn: Turn) -> String? {
        switch turn.parts.last {
        case .text(let event), .thinking(let event): event.id
        case .tools(_, let calls): calls.last.flatMap { ($0.result ?? $0.use)?.id }
        case nil: nil
        }
    }
}

extension Turn {
    /// A tool event joins the run of tool calls at the end of the turn, or opens
    /// one. A result that carries a call id (hub #3797) closes the call with
    /// that id wherever it sits in the turn, so parallel calls whose results
    /// come back out of order still pair exactly; a result whose id matches no
    /// open call here (its use is on an older page) stands alone and never
    /// takes another call's place. A result without an id (an older hub, or an
    /// id over the wire bound) closes the oldest call still waiting for one,
    /// preferring a call that has no id either; a result with none waiting
    /// stands alone.
    fileprivate mutating func addTool(_ event: ChatEvent) {
        if event.phase == .result, let id = event.callId, closeCall(id, with: event) { return }
        var calls: [ToolCall] = []
        var groupID = event.id
        if case .tools(let id, let existing)? = parts.last {
            calls = existing
            groupID = id
            parts.removeLast()
        }
        switch event.phase {
        case .result:
            let open =
                event.callId != nil
                ? nil
                : calls.firstIndex(where: { $0.running && $0.use?.callId == nil })
                    ?? calls.firstIndex(where: { $0.running })
            if let open {
                calls[open].result = event
            } else {
                calls.append(ToolCall(use: nil, result: event))
            }
        case .use, nil:
            calls.append(ToolCall(use: event, result: nil))
        }
        parts.append(.tools(id: groupID, calls: calls))
    }

    /// Close the oldest still-waiting call carrying `callId`, in any run of
    /// this turn. False when there is none.
    private mutating func closeCall(_ callId: String, with result: ChatEvent) -> Bool {
        for index in parts.indices {
            guard case .tools(let id, var calls) = parts[index],
                let open = calls.firstIndex(where: { $0.running && $0.use?.callId == callId })
            else { continue }
            calls[open].result = result
            parts[index] = .tools(id: id, calls: calls)
            return true
        }
        return false
    }
}

// MARK: Folding

/// Which folded things the reader opened. Everything starts folded.
struct Disclosure: Sendable, Equatable {
    private(set) var open: Set<String> = []

    func isOpen(_ id: String) -> Bool { open.contains(id) }

    mutating func toggle(_ id: String) {
        if open.contains(id) { open.remove(id) } else { open.insert(id) }
    }
}

/// The folded row of a run of tool calls: "Used Bash, Read · 3 calls".
func toolSummary(_ calls: [ToolCall]) -> String {
    var names: [String] = []
    for call in calls where !names.contains(call.name) { names.append(call.name) }
    let shown = names.prefix(3).joined(separator: ", ") + (names.count > 3 ? " +\(names.count - 3)" : "")
    let running = calls.contains { $0.running }
    let verb = running ? "Using" : "Used"
    if calls.count == 1 { return "\(verb) \(shown)" }
    return "\(verb) \(shown) · \(calls.count) calls"
}

/// The first lines of a long text, for a folded body, and whether anything was cut.
func clippedPreview(_ text: String, maxLines: Int = 6, maxCharacters: Int = 400) -> (text: String, cut: Bool) {
    var lines: [Substring] = []
    var cut = false
    var used = 0
    for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
        if lines.count == maxLines || used >= maxCharacters { cut = true; break }
        let room = maxCharacters - used
        if line.count > room {
            lines.append(line.prefix(room))
            cut = true
            break
        }
        lines.append(line)
        used += line.count + 1
    }
    return (lines.joined(separator: "\n"), cut)
}

// MARK: Following the tail

/// Whether the view should keep scrolling to new output (the reader is at the
/// bottom) or leave them where they are and offer a jump to the latest.
struct TailFollow: Sendable, Equatable {
    /// Within this many points of the bottom still counts as at the bottom.
    static let slack: Double = 80

    private(set) var atBottom = true
    /// Events that arrived while the reader was reading above.
    private(set) var unseen = 0
    private var newestID: String?
    private var eventCount = 0
    private var seeded = false

    private var contentHeight: Double?

    /// The scroll geometry changed; `distanceFromBottom` is the content below the
    /// viewport. When `contentHeight` also changed, the content grew or shrank
    /// under a reader who did not move (new output, rows measured), so whether
    /// they were at the bottom is kept as it was: `update` decides what to do.
    mutating func scrolled(distanceFromBottom: Double, contentHeight: Double? = nil) {
        defer { if contentHeight != nil { self.contentHeight = contentHeight } }
        if let contentHeight, let known = self.contentHeight, abs(contentHeight - known) > 0.5 { return }
        atBottom = distanceFromBottom <= Self.slack
        if atBottom { unseen = 0 }
    }

    /// The transcript changed. Returns whether to scroll to the latest now:
    /// the first content, or new output while the reader is at the bottom.
    /// Older pages loading above change the count but not the newest event, so
    /// they never move the reader.
    mutating func update(newestID: String?, eventCount: Int) -> Bool {
        defer {
            self.newestID = newestID
            self.eventCount = eventCount
        }
        guard seeded else {
            seeded = newestID != nil
            return newestID != nil
        }
        guard newestID != self.newestID else { return false }
        if atBottom { return true }
        unseen += max(1, eventCount - self.eventCount)
        return false
    }

    /// The reader asked for the latest (or sent a message).
    mutating func jumpedToLatest() {
        atBottom = true
        unseen = 0
    }
}

// MARK: The outbox in the conversation

/// The outbox messages that still belong at the bottom of the chat as pending
/// bubbles, oldest first: everything not yet delivered, and delivered messages
/// the conversation does not show yet. A delivered message is the
/// conversation's own bubble, and leaves the pending list, once the
/// conversation carries it (an item with its hub id, or with the key the phone
/// sent, hub #3797) or the hub's delivery record for it reads `delivered`
/// (matched by that key, else by its hub id). The record covers a message the
/// conversation shows as a system note without either id (#508) or has pushed
/// off the newest page behind tool events.
func pendingOutgoing(
    _ outbox: [OutboxMessage], conversation: [ConversationItem],
    deliveries: [OperatorMessageState] = []
) -> [OutboxMessage] {
    let shownIDs = Set(conversation.compactMap(\.operatorMessageId))
    let shownKeys = Set(conversation.compactMap { $0.idempotencyKey?.lowercased() })
    return outbox
        .filter { message in
            guard message.state == .delivered else { return true }
            if shownKeys.contains(message.id.lowercased()) { return false }
            if let hubID = message.hubMessageId, shownIDs.contains(hubID) { return false }
            return !deliveries.contains { $0.isDelivered && $0.describes(message) }
        }
        .sorted { $0.createdAt < $1.createdAt }
}

// MARK: Rows

/// One row of the lazy list. A turn is split into its pieces so a long
/// assistant turn is many small rows the list can skip, not one tall one.
struct ChatRow: Sendable, Hashable, Identifiable {
    enum Kind: Sendable, Hashable {
        /// The assistant's name and the time, above its first piece.
        case header
        case part(TurnPart)
        /// The time under the operator's bubbles or a system note.
        case stamp
    }

    let turnID: String
    let role: TurnRole
    let createdAt: String
    let kind: Kind
    /// The first row of its turn: it gets the larger gap above.
    let startsTurn: Bool

    var id: String {
        switch kind {
        case .header: "header:" + turnID
        case .part(let part): part.id
        case .stamp: "stamp:" + turnID
        }
    }
}

extension ChatTranscript {
    /// The rows of `turns`, in order. Row ids are unique and stay put while
    /// newer output appends.
    static func rows(_ turns: [Turn]) -> [ChatRow] {
        var rows: [ChatRow] = []
        for turn in turns {
            func row(_ kind: ChatRow.Kind) -> ChatRow {
                ChatRow(
                    turnID: turn.id, role: turn.role, createdAt: turn.createdAt, kind: kind,
                    startsTurn: false)
            }
            var made: [ChatRow] = []
            if turn.role == .assistant { made.append(row(.header)) }
            made += turn.parts.map { row(.part($0)) }
            if turn.role != .assistant { made.append(row(.stamp)) }
            if let first = made.first {
                made[0] = ChatRow(
                    turnID: first.turnID, role: first.role, createdAt: first.createdAt, kind: first.kind,
                    startsTurn: true)
            }
            rows += made
        }
        return rows
    }
}
