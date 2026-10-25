import Foundation
import Observation

// The outbox (#505): a message typed in the app is written to disk before
// anything is sent, shown as queued / sending / delivered / failed, and
// retried with jittered exponential backoff until the hub accepts it, across
// app restarts. The client message id is the idempotency key of every try, so
// a retry never queues a second message (the hub replays the stored row:
// docs/rsi-remote/contracts-v2.md "Idempotency").

/// Where one outbox message stands.
enum OutboxState: String, Codable, Sendable {
    /// Saved, waiting for its turn or its next retry. Nothing needs the operator.
    case queued
    /// A send is in flight now.
    case sending
    /// The hub accepted it (its own delivery state is `hubState`).
    case delivered
    /// The hub refused it for good: only the operator can act (send again or discard).
    case failed
}

/// One message in the outbox. `id` is the client message id: it is created
/// once, saved with the text, and is the `idempotency_key` of every attempt.
struct OutboxMessage: Codable, Sendable, Hashable, Identifiable {
    var id: String
    /// The gateway host the message was written for; it is only sent to that host.
    let host: String
    let project: String
    /// `SeatRole.rawValue`, so a rotated seat can be followed.
    let role: String
    var seat: String
    let text: String
    let createdAt: Date
    var state = OutboxState.queued
    /// Failed attempts in a row since the last reset; the backoff exponent.
    var failures = 0
    /// Earliest time of the next attempt; nil: as soon as it is its turn.
    var nextAttemptAt: Date?
    var lastError: String?
    var lastErrorKind: String?
    /// Some attempt may have reached the hub (lost connection, 502/504 unknown,
    /// the app was closed mid-send). The same id on the next try makes that safe.
    var mayHaveSent = false
    var hubMessageId: String?
    var hubState: String?
    /// The hub answered the retry with the stored row of an earlier try.
    var deduplicated = false
    var deliveredAt: Date?

    init(
        id: String, host: String, project: String, role: String, seat: String, text: String,
        createdAt: Date
    ) {
        self.id = id
        self.host = host
        self.project = project
        self.role = role
        self.seat = seat
        self.text = text
        self.createdAt = createdAt
    }

    /// The frozen request of every attempt: same text, same key.
    var request: MessageRequest {
        MessageRequest(project: project, seat: seat, text: text, idempotencyKey: id)
    }

    /// The seat a retarget groups by: messages to one seat go out in order.
    fileprivate var lane: String { "\(host)|\(project)|\(seat)" }
}

// MARK: Backoff

/// Retry schedule: 1 s doubling to 5 min, spread ±20 % so many phones do not
/// retry in step, and never earlier than the hub's `Retry-After`.
struct OutboxBackoff: Sendable, Equatable {
    var base: TimeInterval = 1
    var cap: TimeInterval = 300
    /// Half-width of the jitter band around the nominal wait.
    var spread = 0.2
    /// The longest `Retry-After` honoured.
    var maxRetryAfter: TimeInterval = 3600

    /// The wait before the next try after `failures` failures in a row
    /// (1-based): `base`, doubling, up to `cap`.
    func nominal(failures: Int) -> TimeInterval {
        guard failures > 0 else { return 0 }
        return min(cap, base * pow(2, Double(min(failures - 1, 30))))
    }

    /// `jitter` is a draw from 0...1: 0 gives the short end of the band, 1 the
    /// long end. The cap holds after jitter; `retryAfter` is a floor.
    func delay(failures: Int, jitter: Double, retryAfter: Int? = nil) -> TimeInterval {
        let draw = min(max(jitter, 0), 1)
        let factor = 1 - spread + 2 * spread * draw
        let wait = min(cap, nominal(failures: failures) * factor)
        guard let retryAfter, retryAfter > 0 else { return wait }
        return max(wait, min(TimeInterval(retryAfter), maxRetryAfter))
    }
}

/// The time source: `now` and a cancellable sleep. Tests move both by hand.
struct OutboxClock: Sendable {
    var now: @Sendable () -> Date
    var sleep: @Sendable (TimeInterval) async throws -> Void

    static let system = OutboxClock(
        now: { Date() },
        sleep: { try await Task.sleep(for: .seconds($0)) })
}

// MARK: Storage

/// What a load found.
struct OutboxLoad: Sendable {
    var entries: [OutboxMessage] = []
    /// A file that could not be read was kept under this name, not deleted.
    var quarantined: String?
}

protocol OutboxStore: Sendable {
    func load() -> OutboxLoad
    func save(_ entries: [OutboxMessage]) throws
}

/// The outbox as one JSON file, written atomically. Messages are the
/// operator's own text: the file lives in the app's private Application
/// Support folder, readable after the first unlock (a retry may run while the
/// phone is locked), and is left out of backups.
struct FileOutboxStore: OutboxStore {
    let url: URL

    private struct Envelope: Codable {
        var version = 1
        var entries: [OutboxMessage]
    }

    static func standard() -> FileOutboxStore {
        let base =
            (try? FileManager.default.url(
                for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true))
            ?? FileManager.default.temporaryDirectory
        return FileOutboxStore(url: base.appendingPathComponent("RSIRemote/outbox.json"))
    }

    private static func coder() -> (JSONEncoder, JSONDecoder) {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.sortedKeys]
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return (encoder, decoder)
    }

    func load() -> OutboxLoad {
        guard let data = try? Data(contentsOf: url) else { return OutboxLoad() }
        do {
            let envelope = try Self.coder().1.decode(Envelope.self, from: data)
            return OutboxLoad(entries: envelope.entries)
        } catch {
            // Never drop what cannot be read: keep it next to the live file.
            let name = "outbox.unreadable-\(Int(Date().timeIntervalSince1970)).json"
            let kept = url.deletingLastPathComponent().appendingPathComponent(name)
            try? FileManager.default.moveItem(at: url, to: kept)
            return OutboxLoad(quarantined: name)
        }
    }

    func save(_ entries: [OutboxMessage]) throws {
        let folder = url.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let data = try Self.coder().0.encode(Envelope(entries: entries))
        #if os(iOS)
            try data.write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
        #else
            try data.write(to: url, options: .atomic)
        #endif
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        var excluded = url
        try? excluded.setResourceValues(values)
    }
}

/// Keeps the outbox in memory (tests, the harness). Saving goes through the
/// same encoder as the file store, so a round trip proves the persisted shape.
final class MemoryOutboxStore: OutboxStore, @unchecked Sendable {
    private let lock = NSLock()
    private var data: Data?
    private(set) var saves = 0
    var failSaves = false

    private struct Envelope: Codable { var entries: [OutboxMessage] }

    func load() -> OutboxLoad {
        lock.withLock {
            guard let data else { return OutboxLoad() }
            let decoder = JSONDecoder()
            decoder.dateDecodingStrategy = .iso8601
            let entries = (try? decoder.decode(Envelope.self, from: data))?.entries ?? []
            return OutboxLoad(entries: entries)
        }
    }

    func save(_ entries: [OutboxMessage]) throws {
        try lock.withLock {
            if failSaves { throw CocoaError(.fileWriteOutOfSpace) }
            let encoder = JSONEncoder()
            encoder.dateEncodingStrategy = .iso8601
            data = try encoder.encode(Envelope(entries: entries))
            saves += 1
        }
    }
}

// MARK: Outbox

/// What the outbox sends through: the current host and the real client.
@MainActor
protocol OutboxTransport: AnyObject {
    /// The host the app talks to now; messages written for another host wait.
    var activeHost: String? { get }
    /// One attempt (a 403 already refined into messaging-off or seat-changed).
    func deliver(_ request: MessageRequest) async -> Result<MessageReceipt, MessageError>
    /// The seat now filling `role` in `project`, as last listed.
    func currentSeat(project: String, role: String) -> String?
}

@MainActor
@Observable
final class Outbox {
    /// Delivered messages are kept this long (and at most `keepDelivered`) so the
    /// composer can show them; the conversation has them for good.
    static let deliveredFor: TimeInterval = 3600
    static let keepDelivered = 20

    private(set) var entries: [OutboxMessage] = []
    /// Why the outbox file could not be written or read, for a banner.
    private(set) var storageProblem: String?

    @ObservationIgnored weak var transport: (any OutboxTransport)?
    /// Called after a message is accepted by the hub (to refresh its conversation).
    @ObservationIgnored var onDelivered: (@MainActor (OutboxMessage) -> Void)?

    @ObservationIgnored private let store: any OutboxStore
    @ObservationIgnored private let clock: OutboxClock
    @ObservationIgnored private let jitter: @Sendable () -> Double
    @ObservationIgnored private let newID: @Sendable () -> String
    @ObservationIgnored let backoff: OutboxBackoff
    /// Least time between two sends: the gateway allows 1 message per second.
    @ObservationIgnored let minSpacing: TimeInterval
    @ObservationIgnored private var lastSendAt = Date.distantPast
    @ObservationIgnored private var pumping = false
    @ObservationIgnored private var pumpAgain = false
    @ObservationIgnored private var timer: Task<Void, Never>?

    init(
        store: any OutboxStore, clock: OutboxClock = .system,
        jitter: @escaping @Sendable () -> Double = { Double.random(in: 0...1) },
        backoff: OutboxBackoff = OutboxBackoff(), minSpacing: TimeInterval = 1.1,
        newID: @escaping @Sendable () -> String = { UUID().uuidString.lowercased() }
    ) {
        self.store = store
        self.clock = clock
        self.jitter = jitter
        self.backoff = backoff
        self.minSpacing = minSpacing
        self.newID = newID
        let loaded = store.load()
        if let quarantined = loaded.quarantined {
            storageProblem = "The saved outbox could not be read; it was kept as \(quarantined)."
        }
        entries = loaded.entries.map { entry in
            var entry = entry
            // The app ended mid-send: the request may have left. Same id next time.
            if entry.state == .sending {
                entry.state = .queued
                entry.mayHaveSent = true
                entry.nextAttemptAt = nil
            }
            return entry
        }
        prune()
    }

    // MARK: Queries

    func messages(host: String, project: String) -> [OutboxMessage] {
        entries.filter { $0.host == host && $0.project == project }
            .sorted { $0.createdAt != $1.createdAt ? $0.createdAt > $1.createdAt : $0.id > $1.id }
    }

    func message(_ id: String) -> OutboxMessage? { entries.first { $0.id == id } }

    /// Messages still on their way (queued or sending).
    var unsentCount: Int { entries.filter { $0.state == .queued || $0.state == .sending }.count }

    // MARK: Changes

    /// Save the message, then start sending it. Throws (nothing saved) for a
    /// text the gateway would refuse.
    @discardableResult
    func enqueue(host: String, project: String, role: String, seat: String, text: String) throws(MessageError)
        -> OutboxMessage
    {
        if let problem = messageProblem(text) { throw MessageError(.invalid, problem) }
        let entry = OutboxMessage(
            id: newID(), host: host, project: project, role: role, seat: seat, text: text,
            createdAt: clock.now())
        entries.append(entry)
        persist()
        startPump()
        return entry
    }

    /// Try a waiting message now instead of at its backoff time.
    func retryNow(_ id: String) {
        update(id) { entry in
            guard entry.state == .queued else { return }
            entry.nextAttemptAt = clock.now()
        }
        startPump()
    }

    /// Put a failed message back in the queue. A key the hub refused for this
    /// text (conflict) is replaced; any other keeps its id.
    func sendAgain(_ id: String) {
        update(id) { entry in
            guard entry.state == .failed else { return }
            if entry.lastErrorKind == MessageErrorKind.conflict.rawValue { entry.id = newID() }
            entry.state = .queued
            entry.failures = 0
            entry.nextAttemptAt = clock.now()
        }
        startPump()
    }

    /// Give up on a message. Not while it is being sent.
    func discard(_ id: String) {
        guard let entry = message(id), entry.state != .sending else { return }
        entries.removeAll { $0.id == id }
        persist()
        scheduleTimer()
    }

    /// The gateway now says messaging is on: held messages try again at once.
    func messagingEnabled() {
        var changed = false
        for index in entries.indices
        where entries[index].state == .queued
            && entries[index].lastErrorKind == MessageErrorKind.messagingOff.rawValue
        {
            entries[index].nextAttemptAt = clock.now()
            changed = true
        }
        if changed {
            persist()
            startPump()
        }
    }

    /// The app came to the foreground, the host changed or a screen opened:
    /// look at the queue now.
    func kick() { startPump() }

    /// Stop the retry timer (the outbox is going away, or a test is playing a
    /// kill). Messages stay saved; the next `kick` starts again.
    func stop() {
        timer?.cancel()
        timer = nil
    }

    private func update(_ id: String, _ change: (inout OutboxMessage) -> Void) {
        guard let index = entries.firstIndex(where: { $0.id == id }) else { return }
        change(&entries[index])
        persist()
    }

    private func persist() {
        do {
            try store.save(entries)
            storageProblem = nil
        } catch {
            storageProblem =
                "The outbox could not be saved (\(error.localizedDescription)); queued messages would be lost if the app closes."
        }
    }

    private func prune() {
        let now = clock.now()
        var delivered = entries.filter { $0.state == .delivered }
            .sorted { ($0.deliveredAt ?? .distantPast) > ($1.deliveredAt ?? .distantPast) }
        delivered = Array(
            delivered.prefix(Self.keepDelivered).filter {
                now.timeIntervalSince($0.deliveredAt ?? .distantPast) < Self.deliveredFor
            })
        let keep = Set(delivered.map(\.id))
        entries.removeAll { $0.state == .delivered && !keep.contains($0.id) }
    }

    // MARK: Sending

    /// The queued messages of the active host in send order, each with the
    /// time it may go out. A message waits for every earlier one to the same
    /// seat, so a busy hub never reorders what the operator wrote.
    func plan() -> [(id: String, due: Date)] {
        guard let host = transport?.activeHost else { return [] }
        var laneDue: [String: Date] = [:]
        var order: [(id: String, due: Date)] = []
        let spacing = lastSendAt.addingTimeInterval(minSpacing)
        for entry in entries.sorted(by: { ($0.createdAt, $0.id) < ($1.createdAt, $1.id) })
        where entry.host == host && entry.state == .queued {
            // A fresh message keeps the gateway's 1-per-second spacing; a retry has its own time.
            let own = entry.nextAttemptAt ?? max(entry.createdAt, spacing)
            let due = max(own, laneDue[entry.lane] ?? .distantPast)
            laneDue[entry.lane] = due
            order.append((entry.id, due))
        }
        return order
    }

    private func startPump() {
        Task { await pump() }
    }

    /// Send every message that is due, one at a time, then arm the timer for
    /// the next. Safe to call at any time: one pump runs at once.
    func pump() async {
        if pumping {
            pumpAgain = true
            return
        }
        pumping = true
        defer { pumping = false }
        repeat {
            pumpAgain = false
            while let next = plan().filter({ $0.due <= clock.now() }).min(by: { $0.due < $1.due }) {
                await attempt(next.id)
            }
        } while pumpAgain
        prune()
        scheduleTimer()
    }

    private func scheduleTimer() {
        timer?.cancel()
        timer = nil
        guard let due = plan().map(\.due).min() else { return }
        let wait = max(0, due.timeIntervalSince(clock.now()))
        let clock = self.clock
        timer = Task { [weak self] in
            do { try await clock.sleep(wait) } catch { return }
            await self?.pump()
        }
    }

    private func attempt(_ id: String) async {
        guard let transport, let entry = message(id) else { return }
        update(id) { $0.state = .sending }
        lastSendAt = clock.now()
        let result = await transport.deliver(entry.request)
        lastSendAt = clock.now()
        guard let current = message(id) else { return }
        switch result {
        case .success(let receipt):
            update(id) { entry in
                entry.state = .delivered
                entry.deliveredAt = clock.now()
                entry.hubMessageId = receipt.messageId
                entry.hubState = receipt.state
                entry.deduplicated = receipt.deduplicated
                entry.lastError = nil
                entry.lastErrorKind = nil
                entry.nextAttemptAt = nil
            }
            if let delivered = message(id) { onDelivered?(delivered) }
        case .failure(let error):
            let retarget =
                error.kind == .seatChanged
                ? transport.currentSeat(project: current.project, role: current.role) : nil
            let wait = backoff.delay(
                failures: current.failures + 1, jitter: jitter(), retryAfter: error.retryAfter)
            update(id) { entry in
                entry.lastError = error.message
                entry.lastErrorKind = error.kind.rawValue
                if error.mayHaveSent { entry.mayHaveSent = true }
                switch error.kind {
                case .invalid, .conflict:
                    // The hub will never take this as written.
                    entry.state = .failed
                case .seatChanged where retarget != nil && retarget != entry.seat:
                    // The manager rotated: the same message goes to the seat that took over.
                    entry.seat = retarget ?? entry.seat
                    entry.state = .queued
                    entry.nextAttemptAt = clock.now()
                default:
                    entry.state = .queued
                    entry.failures += 1
                    entry.nextAttemptAt = clock.now().addingTimeInterval(wait)
                }
            }
        }
    }
}

// MARK: Display

/// What an outbox row says: a short state word and the reason for any wait,
/// plainly (never a bare "error"). `reason` is nil when nothing needs saying.
struct OutboxDescription: Equatable, Sendable {
    let state: String
    let tone: AnswerTone
    let reason: String?
    /// The message may have reached the hub already; resending is safe.
    let note: String?
}

func describeOutbox(_ message: OutboxMessage) -> OutboxDescription {
    let note =
        message.mayHaveSent
        ? "An earlier try may have arrived. Every try carries the same message id, so the hub drops a repeat."
        : nil
    switch message.state {
    case .sending:
        return OutboxDescription(state: "Sending…", tone: .busy, reason: nil, note: note)
    case .delivered:
        return OutboxDescription(state: "Delivered to the hub", tone: .ok, reason: nil, note: nil)
    case .failed:
        return OutboxDescription(
            state: "Not sent", tone: .error, reason: message.lastError, note: nil)
    case .queued:
        return OutboxDescription(
            state: message.lastError == nil ? "Queued" : "Queued, will retry",
            tone: message.lastError == nil ? .busy : .warn,
            reason: message.lastError.map { _ in waitingReason(message) }, note: note)
    }
}

/// Why a queued message is waiting, by what the last try met.
private func waitingReason(_ message: OutboxMessage) -> String {
    switch message.lastErrorKind.flatMap(MessageErrorKind.init(rawValue:)) {
    case .unavailable: "The hub can't be reached right now."
    case .busy: "The hub is busy."
    case .rateLimited: "Sending too fast for the hub; waiting."
    case .unknown: "The hub did not confirm the send."
    case .session: "Signing in to the hub failed."
    case .messagingOff: "Messaging from the phone is off on the hub. It sends as soon as it is turned on."
    case .forbidden: "The hub refused this device (403)."
    case .seatChanged: "That manager seat changed; waiting for the new one."
    case .unexpected: "The hub returned an error."
    case .invalid, .conflict, nil: message.lastError ?? "Waiting."
    }
}
