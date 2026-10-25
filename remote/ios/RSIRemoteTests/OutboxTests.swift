import Foundation
import XCTest

@testable import RSIRemote

// The outbox (#505): backoff schedule with an injected clock and jitter source,
// persistence round trip, state transitions, and no duplicate on retry (the
// idempotency key is the message id on every attempt), first against a scripted
// transport, then through AppModel and the real RemoteClient over the stub gateway.

private let obHost = "hub.example.ts.net"
private let obProject = "550e8400-e29b-41d4-a716-446655440000"
private let obPM = "550e8400-e29b-41d4-a716-446655440001"
private let obGlobal = "550e8400-e29b-41d4-a716-446655440002"
private let obMovedPM = "550e8400-e29b-41d4-a716-4466554400aa"

/// A clock the test moves by hand. Sleepers wake when `advance` passes their deadline.
final class ManualClock: @unchecked Sendable {
    private let lock = NSLock()
    private var current: Date
    private var nextID = 0
    private var sleepers: [Int: (deadline: Date, cont: CheckedContinuation<Void, any Error>)] = [:]
    private var cancelled: Set<Int> = []
    /// Every sleep asked for, in seconds, in order.
    private(set) var requested: [TimeInterval] = []

    init(_ start: Date = Date(timeIntervalSince1970: 1_700_000_000)) { current = start }

    var now: Date { lock.withLock { current } }

    /// Sleepers waiting right now.
    var sleeping: Int { lock.withLock { sleepers.count } }

    var clock: OutboxClock {
        OutboxClock(now: { self.now }, sleep: { try await self.sleep($0) })
    }

    func sleep(_ seconds: TimeInterval) async throws {
        let id = lock.withLock { () -> Int in
            nextID += 1
            requested.append(seconds)
            return nextID
        }
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, any Error>) in
                let action: (() -> Void)? = lock.withLock {
                    if cancelled.contains(id) { return { cont.resume(throwing: CancellationError()) } }
                    let deadline = current.addingTimeInterval(seconds)
                    if seconds <= 0 { return { cont.resume() } }
                    sleepers[id] = (deadline, cont)
                    return nil
                }
                action?()
            }
        } onCancel: {
            let cont = lock.withLock { () -> CheckedContinuation<Void, any Error>? in
                cancelled.insert(id)
                return sleepers.removeValue(forKey: id)?.cont
            }
            cont?.resume(throwing: CancellationError())
        }
    }

    func advance(_ seconds: TimeInterval) {
        let due = lock.withLock { () -> [CheckedContinuation<Void, any Error>] in
            current = current.addingTimeInterval(seconds)
            let ready = sleepers.filter { $0.value.deadline <= current }
            for id in ready.keys { sleepers[id] = nil }
            return ready.values.map(\.cont)
        }
        for cont in due { cont.resume() }
    }
}

private func receipt(
    state: String = "queued", deduplicated: Bool = false, id: String = "550e8400-e29b-41d4-a716-44665544000a"
) throws -> MessageReceipt {
    let json = """
        {"message_id":"\(id)","seat_session_id":"\(obPM)","state":"\(state)",\
        "created_at":"2026-10-22T09:31:00.000000000Z","deduplicated":\(deduplicated)}
        """
    return try JSONDecoder().decode(MessageReceipt.self, from: Data(json.utf8))
}

/// A transport that answers from a script (the last answer repeats) and records every request.
@MainActor
final class ScriptedTransport: OutboxTransport {
    var activeHost: String? = obHost
    var script: [Result<MessageReceipt, MessageError>] = []
    var seats: [String: String] = [:]
    private(set) var sent: [MessageRequest] = []
    /// When set, `deliver` waits here before answering (a send in flight).
    var hold: (() async -> Void)?
    var inFlight: (@MainActor () -> Void)?

    func deliver(_ request: MessageRequest) async -> Result<MessageReceipt, MessageError> {
        sent.append(request)
        inFlight?()
        if let hold { await hold() }
        guard !script.isEmpty else { return .failure(MessageError(.unavailable, "no script")) }
        return script.count > 1 ? script.removeFirst() : script[0]
    }

    func currentSeat(project: String, role: String) -> String? { seats["\(project)|\(role)"] }
}

private let unavailable = MessageError(.unavailable, "Can't reach the RSI host. Nothing was sent.")

@MainActor
class OutboxTestCase: XCTestCase {
    let time = ManualClock()
    let store = MemoryOutboxStore()
    let transport = ScriptedTransport()
    private var counter = 0

    func makeOutbox(
        store: (any OutboxStore)? = nil, transport: ScriptedTransport? = nil, jitter: Double = 0.5,
        minSpacing: TimeInterval = 1.1
    ) -> Outbox {
        let ids = IDSource()
        let outbox = Outbox(
            store: store ?? self.store, clock: time.clock, jitter: { jitter }, minSpacing: minSpacing,
            newID: { ids.next() })
        outbox.transport = transport ?? self.transport
        return outbox
    }

    /// Wait (without moving the clock) until `condition` holds.
    func eventually(
        _ what: String, _ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line
    ) async {
        for _ in 0..<400 {
            if condition() { return }
            try? await Task.sleep(for: .milliseconds(5))
        }
        XCTFail("timed out waiting for: \(what)", file: file, line: line)
    }

    func enqueue(_ outbox: Outbox, _ text: String = "ship it", seat: String = obPM) throws -> OutboxMessage {
        try outbox.enqueue(host: obHost, project: obProject, role: "project_manager", seat: seat, text: text)
    }
}

/// Deterministic ids shaped like the lowercase UUIDs the gateway wants.
final class IDSource: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func next() -> String {
        lock.withLock {
            n += 1
            return String(format: "00000000-0000-4000-8000-%012d", n)
        }
    }
}

// MARK: Backoff

final class OutboxBackoffTests: XCTestCase {
    func testTheNominalScheduleDoublesFromOneSecondToFiveMinutes() {
        let backoff = OutboxBackoff()
        let schedule = (1...12).map { backoff.nominal(failures: $0) }
        XCTAssertEqual(schedule, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300])
        XCTAssertEqual(backoff.nominal(failures: 0), 0)
        XCTAssertEqual(backoff.nominal(failures: 10_000), 300)
    }

    func testJitterSpreadsEachWaitAroundTheNominalAndNeverPastTheCap() {
        let backoff = OutboxBackoff()
        XCTAssertEqual(backoff.delay(failures: 3, jitter: 0), 3.2, accuracy: 1e-9)
        XCTAssertEqual(backoff.delay(failures: 3, jitter: 0.5), 4, accuracy: 1e-9)
        XCTAssertEqual(backoff.delay(failures: 3, jitter: 1), 4.8, accuracy: 1e-9)
        // A draw outside 0...1 is clamped.
        XCTAssertEqual(backoff.delay(failures: 3, jitter: 7), 4.8, accuracy: 1e-9)
        XCTAssertEqual(backoff.delay(failures: 3, jitter: -3), 3.2, accuracy: 1e-9)
        // At the cap the jitter only ever shortens.
        XCTAssertEqual(backoff.delay(failures: 20, jitter: 1), 300, accuracy: 1e-9)
        XCTAssertEqual(backoff.delay(failures: 20, jitter: 0), 240, accuracy: 1e-9)
    }

    func testRetryAfterIsAFloorAndIsBounded() {
        let backoff = OutboxBackoff()
        XCTAssertEqual(backoff.delay(failures: 1, jitter: 0.5, retryAfter: 90), 90, accuracy: 1e-9)
        XCTAssertEqual(backoff.delay(failures: 9, jitter: 0.5, retryAfter: 2), 256, accuracy: 1e-9)
        XCTAssertEqual(backoff.delay(failures: 1, jitter: 0.5, retryAfter: 99_999), 3600, accuracy: 1e-9)
        XCTAssertEqual(backoff.delay(failures: 1, jitter: 0.5, retryAfter: 0), 1, accuracy: 1e-9)
    }
}

// MARK: Schedule, states, ordering

@MainActor
final class OutboxScheduleTests: OutboxTestCase {
    func testAFailingHubIsRetriedOnTheDoublingScheduleUntilItAnswers() async throws {
        transport.script = [.failure(unavailable)]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        await eventually("first attempt") { self.transport.sent.count == 1 }
        await eventually("first wait armed") { self.time.requested.count == 1 }
        // Jitter 0.5 is the nominal wait: 1, 2, 4, 8, 16 s.
        var waits: [TimeInterval] = []
        for attempt in 2...6 {
            let wait = try XCTUnwrap(time.requested.last)
            waits.append(wait)
            time.advance(wait)
            await eventually("attempt \(attempt)") { self.transport.sent.count == attempt }
            await eventually("wait \(attempt) armed") { self.time.requested.count == attempt }
        }
        XCTAssertEqual(waits, [1, 2, 4, 8, 16])
        let waiting = try XCTUnwrap(outbox.message(message.id))
        XCTAssertEqual(waiting.state, .queued)
        XCTAssertEqual(waiting.failures, 6)
        XCTAssertEqual(waiting.nextAttemptAt, time.now.addingTimeInterval(32))
        // The hub comes back: the next try delivers, with no action from the operator.
        transport.script = [.success(try receipt())]
        time.advance(32)
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
        XCTAssertEqual(transport.sent.count, 7)
        XCTAssertNil(outbox.message(message.id)?.nextAttemptAt)
        XCTAssertNil(outbox.message(message.id)?.lastError)
    }

    func testTheWaitStopsGrowingAtFiveMinutes() async throws {
        transport.script = [.failure(unavailable)]
        let outbox = makeOutbox()
        _ = try enqueue(outbox)
        var last: TimeInterval = 0
        for attempt in 1...14 {
            await eventually("attempt \(attempt)") { self.transport.sent.count == attempt }
            await eventually("wait armed") { self.time.requested.count == attempt }
            last = try XCTUnwrap(time.requested.last)
            time.advance(last)
        }
        XCTAssertEqual(last, 300)
        XCTAssertLessThanOrEqual(try XCTUnwrap(time.requested.max()), 300)
    }

    func testTheJitterSourceDecidesWhereInTheBandTheWaitFalls() async throws {
        transport.script = [.failure(unavailable)]
        let short = makeOutbox(store: MemoryOutboxStore(), jitter: 0)
        _ = try enqueue(short)
        await eventually("short wait") { self.time.requested.count == 1 }
        XCTAssertEqual(time.requested[0], 0.8, accuracy: 1e-3)  // Date precision at this epoch
    }

    func testRetryAfterFromTheHubIsHonoured() async throws {
        transport.script = [.failure(messageRefusal(status: 429, code: "rate_limited", retryAfter: 45))]
        let outbox = makeOutbox()
        _ = try enqueue(outbox)
        await eventually("wait armed") { self.time.requested.count == 1 }
        XCTAssertEqual(time.requested[0], 45, accuracy: 1e-9)
    }

    func testAMessageGoesThroughQueuedSendingDelivered() async throws {
        let gate = Gate()
        transport.hold = { await gate.wait() }
        transport.script = [.success(try receipt())]
        var states: [OutboxState] = []
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        states.append(try XCTUnwrap(outbox.message(message.id)).state)  // queued: saved before any send
        await eventually("sending") { outbox.message(message.id)?.state == .sending }
        states.append(.sending)
        // Not discardable while in flight.
        outbox.discard(message.id)
        XCTAssertNotNil(outbox.message(message.id))
        await gate.open()
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
        states.append(.delivered)
        XCTAssertEqual(states, [.queued, .sending, .delivered])
        let done = try XCTUnwrap(outbox.message(message.id))
        XCTAssertEqual(done.hubState, "queued")
        XCTAssertEqual(done.hubMessageId, "550e8400-e29b-41d4-a716-44665544000a")
        XCTAssertEqual(done.deliveredAt, time.now)
        XCTAssertEqual(outbox.unsentCount, 0)
    }

    func testTheMessageIsSavedBeforeAnySend() async throws {
        let gate = Gate()
        transport.hold = { await gate.wait() }
        transport.script = [.success(try receipt())]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        // Synchronously after enqueue, before the send has even started: on disk.
        XCTAssertEqual(store.load().entries.map(\.id), [message.id])
        XCTAssertEqual(store.load().entries.first?.state, .queued)
        await gate.open()
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
    }

    func testARefusedTextIsNotSaved() {
        let outbox = makeOutbox()
        XCTAssertThrowsError(try enqueue(outbox, "   ")) { error in
            XCTAssertEqual((error as? MessageError)?.kind, .invalid)
        }
        XCTAssertThrowsError(try enqueue(outbox, String(repeating: "x", count: maxMessageBytes + 1)))
        XCTAssertTrue(outbox.entries.isEmpty)
        XCTAssertEqual(store.saves, 0)
    }

    func testAHardRefusalFailsAndSendAgainRequeues() async throws {
        transport.script = [.failure(messageRefusal(status: 400, code: "invalid_message", retryAfter: nil))]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        await eventually("failed") { outbox.message(message.id)?.state == .failed }
        XCTAssertEqual(transport.sent.count, 1)
        // A failed message is not retried by itself.
        time.advance(3600)
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(transport.sent.count, 1)
        transport.script = [.success(try receipt())]
        outbox.sendAgain(message.id)
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
        XCTAssertEqual(transport.sent.map(\.idempotencyKey), [message.id, message.id])
    }

    func testAKeyConflictTakesANewIdOnSendAgain() async throws {
        transport.script = [.failure(messageRefusal(status: 409, code: "idempotency_conflict", retryAfter: nil))]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        await eventually("failed") { outbox.message(message.id)?.state == .failed }
        transport.script = [.success(try receipt())]
        outbox.sendAgain(message.id)
        await eventually("delivered") { outbox.entries.first?.state == .delivered }
        XCTAssertEqual(outbox.entries.count, 1)
        XCTAssertNotEqual(outbox.entries[0].id, message.id)
        XCTAssertEqual(transport.sent.last?.idempotencyKey, outbox.entries[0].id)
    }

    func testDiscardRemovesAWaitingMessageAndTheSaveFollows() async throws {
        transport.script = [.failure(unavailable)]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        await eventually("waiting") { outbox.message(message.id)?.failures == 1 }
        outbox.discard(message.id)
        XCTAssertTrue(outbox.entries.isEmpty)
        XCTAssertTrue(store.load().entries.isEmpty)
        // Its timer is gone: nothing is sent later.
        time.advance(3600)
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(transport.sent.count, 1)
    }

    func testTryNowSkipsTheWait() async throws {
        transport.script = [.failure(unavailable)]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        await eventually("waiting") { outbox.message(message.id)?.failures == 1 }
        transport.script = [.success(try receipt())]
        outbox.retryNow(message.id)
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
        XCTAssertEqual(transport.sent.count, 2)
    }

    func testMessagesToOneSeatKeepTheirOrderWhileAnEarlierOneWaits() async throws {
        transport.script = [.failure(unavailable)]
        let outbox = makeOutbox()
        let first = try enqueue(outbox, "first")
        await eventually("first waiting") { outbox.message(first.id)?.failures == 1 }
        await eventually("first retry armed") { self.time.sleeping == 1 }
        let second = try enqueue(outbox, "second")
        let other = try enqueue(outbox, "to the global seat", seat: obGlobal)
        transport.script = [.failure(unavailable), .success(try receipt())]
        await eventually("both queued behind the spacing") { self.time.sleeping == 1 && outbox.plan().count == 3 }
        // The first one's retry (1 s) fails again, then the global lane's turn comes (1.1 s after).
        time.advance(1)
        await eventually("first fails again") { outbox.message(first.id)?.failures == 2 }
        await eventually("next armed") { self.time.sleeping == 1 }
        time.advance(1.2)
        // The global seat is its own lane: it goes out; the PM's second waits for the first.
        await eventually("other lane delivered") { outbox.message(other.id)?.state == .delivered }
        XCTAssertEqual(outbox.message(second.id)?.state, .queued)
        XCTAssertEqual(outbox.message(second.id)?.failures, 0)
        XCTAssertFalse(transport.sent.contains { $0.text == "second" })
        // The hub answers: first, then second, in the order written.
        transport.script = [.success(try receipt())]
        await eventually("retry armed") { self.time.sleeping == 1 }
        time.advance(5)
        await eventually("first delivered") { outbox.message(first.id)?.state == .delivered }
        await eventually("second armed behind the spacing") { self.time.sleeping == 1 }
        time.advance(1.2)
        await eventually("both delivered") {
            outbox.message(first.id)?.state == .delivered && outbox.message(second.id)?.state == .delivered
        }
        let texts = transport.sent.map(\.text)
        let firstOK = try XCTUnwrap(texts.lastIndex(of: "first"))
        let secondOK = try XCTUnwrap(texts.firstIndex(of: "second"))
        XCTAssertLessThan(firstOK, secondOK)
    }

    func testAMessageForAnotherHostWaitsForThatHost() async throws {
        transport.activeHost = "other.example.ts.net"
        transport.script = [.success(try receipt())]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertTrue(transport.sent.isEmpty)
        XCTAssertEqual(outbox.message(message.id)?.state, .queued)
        transport.activeHost = obHost
        outbox.kick()
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
    }

    func testARotatedSeatIsFollowedWithTheSameMessageId() async throws {
        transport.script = [.failure(.seatChanged(status: 403)), .success(try receipt())]
        transport.seats["\(obProject)|project_manager"] = obMovedPM
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
        XCTAssertEqual(transport.sent.map(\.seat), [obPM, obMovedPM])
        XCTAssertEqual(transport.sent.map(\.idempotencyKey), [message.id, message.id])
        XCTAssertEqual(outbox.message(message.id)?.seat, obMovedPM)
    }

    func testAMessageHeldForMessagingOffGoesOutWhenItIsTurnedOn() async throws {
        transport.script = [.failure(.messagingOff())]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        await eventually("held") { outbox.message(message.id)?.failures == 1 }
        XCTAssertEqual(outbox.message(message.id)?.lastErrorKind, MessageErrorKind.messagingOff.rawValue)
        transport.script = [.success(try receipt())]
        outbox.messagingEnabled()
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
    }

    func testDeliveredMessagesAreKeptBrieflyThenPruned() async throws {
        transport.script = [.success(try receipt())]
        let outbox = makeOutbox()
        let message = try enqueue(outbox)
        await eventually("delivered") { outbox.message(message.id)?.state == .delivered }
        time.advance(Outbox.deliveredFor + 1)
        // The next load (an app launch) drops it.
        let reopened = makeOutbox(transport: ScriptedTransport())
        XCTAssertTrue(reopened.entries.isEmpty)
    }

    func testAFailingSaveIsReportedNotSilent() throws {
        store.failSaves = true
        let outbox = makeOutbox()
        _ = try enqueue(outbox)
        XCTAssertNotNil(outbox.storageProblem)
        XCTAssertTrue(try XCTUnwrap(outbox.storageProblem).contains("could not be saved"))
    }
}

/// Lets a test hold a send in flight.
actor Gate {
    private var isOpen = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func wait() async {
        if isOpen { return }
        await withCheckedContinuation { waiters.append($0) }
    }

    func open() {
        isOpen = true
        for waiter in waiters { waiter.resume() }
        waiters = []
    }
}

// MARK: Persistence

@MainActor
final class OutboxPersistenceTests: OutboxTestCase {
    func testAQueuedMessageSurvivesARestartWithItsIdAndSchedule() async throws {
        transport.script = [.failure(unavailable)]
        let first = makeOutbox()
        let message = try enqueue(first, "still here after the restart")
        await eventually("waiting") { first.message(message.id)?.failures == 1 }
        let before = try XCTUnwrap(first.message(message.id))
        first.stop()

        // The app is killed and reopened: a new Outbox on the same store.
        let reopenedTransport = ScriptedTransport()
        reopenedTransport.script = [.success(try receipt())]
        let reopened = makeOutbox(transport: reopenedTransport)
        let after = try XCTUnwrap(reopened.message(message.id))
        XCTAssertEqual(after.id, before.id)
        XCTAssertEqual(after.text, before.text)
        XCTAssertEqual(after.seat, before.seat)
        XCTAssertEqual(after.host, before.host)
        XCTAssertEqual(after.state, .queued)
        XCTAssertEqual(after.failures, 1)
        XCTAssertEqual(after.nextAttemptAt, before.nextAttemptAt)
        XCTAssertEqual(after.lastErrorKind, before.lastErrorKind)

        // It is delivered on the reopened outbox, under the same id, once its time comes.
        time.advance(1.1)
        reopened.kick()
        await eventually("delivered") { reopened.message(message.id)?.state == .delivered }
        XCTAssertEqual(reopenedTransport.sent.map(\.idempotencyKey), [message.id])
    }

    func testAMessageInFlightWhenTheAppDiedIsResentUnderTheSameId() async throws {
        let gate = Gate()
        transport.hold = { await gate.wait() }
        transport.script = [.failure(unavailable)]
        let first = makeOutbox()
        let message = try enqueue(first)
        await eventually("sending") { first.message(message.id)?.state == .sending }
        // What is on disk at this instant is what survives a kill.
        XCTAssertEqual(store.load().entries.first?.state, .sending)
        first.stop()

        let reopenedTransport = ScriptedTransport()
        reopenedTransport.script = [.success(try receipt(deduplicated: true))]
        let reopened = makeOutbox(transport: reopenedTransport)
        let revived = try XCTUnwrap(reopened.message(message.id))
        XCTAssertEqual(revived.state, .queued)
        XCTAssertTrue(revived.mayHaveSent)
        reopened.kick()
        await eventually("delivered") { reopened.message(message.id)?.state == .delivered }
        XCTAssertEqual(reopenedTransport.sent.map(\.idempotencyKey), [message.id])
        XCTAssertEqual(reopened.message(message.id)?.deduplicated, true)
        await gate.open()
    }

    func testTheFileStoreRoundTripsEveryField() throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("rsi-outbox-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: folder) }
        let file = FileOutboxStore(url: folder.appendingPathComponent("RSIRemote/outbox.json"))
        XCTAssertTrue(file.load().entries.isEmpty)

        var queued = OutboxMessage(
            id: "00000000-0000-4000-8000-000000000001", host: obHost, project: obProject, role: "project_manager",
            seat: obPM, text: "ünïcode \"quoted\" text\nsecond line", createdAt: Date(timeIntervalSince1970: 1_700_000_000))
        queued.failures = 3
        queued.nextAttemptAt = Date(timeIntervalSince1970: 1_700_000_060)
        queued.lastError = "The hub is busy."
        queued.lastErrorKind = MessageErrorKind.busy.rawValue
        queued.mayHaveSent = true
        var delivered = queued
        delivered.id = "00000000-0000-4000-8000-000000000002"
        delivered.state = .delivered
        delivered.hubMessageId = "550e8400-e29b-41d4-a716-44665544000a"
        delivered.hubState = "queued"
        delivered.deduplicated = true
        delivered.deliveredAt = Date(timeIntervalSince1970: 1_700_000_100)
        try file.save([queued, delivered])

        let loaded = file.load()
        XCTAssertNil(loaded.quarantined)
        XCTAssertEqual(loaded.entries, [queued, delivered])
    }

    func testAnUnreadableFileIsKeptNotDeleted() throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("rsi-outbox-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: folder) }
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let url = folder.appendingPathComponent("outbox.json")
        try Data("{ not json".utf8).write(to: url)
        let loaded = FileOutboxStore(url: url).load()
        XCTAssertTrue(loaded.entries.isEmpty)
        let name = try XCTUnwrap(loaded.quarantined)
        XCTAssertEqual(try String(contentsOf: folder.appendingPathComponent(name), encoding: .utf8), "{ not json")
        // The outbox says so instead of starting silently empty.
        let outbox = Outbox(store: FileOutboxStore(url: url))
        XCTAssertNil(outbox.storageProblem)  // the bad file was already moved aside above
    }

    func testAnOutboxOnAnUnreadableFileTellsTheOperator() throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("rsi-outbox-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: folder) }
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let url = folder.appendingPathComponent("outbox.json")
        try Data("{ not json".utf8).write(to: url)
        let outbox = Outbox(store: FileOutboxStore(url: url))
        XCTAssertTrue(try XCTUnwrap(outbox.storageProblem).contains("kept as outbox.unreadable-"))
    }
}

// MARK: Display

final class OutboxDisplayTests: XCTestCase {
    private func message(_ change: (inout OutboxMessage) -> Void = { _ in }) -> OutboxMessage {
        var m = OutboxMessage(
            id: "00000000-0000-4000-8000-000000000001", host: obHost, project: obProject, role: "project_manager",
            seat: obPM, text: "hi", createdAt: Date(timeIntervalSince1970: 1_700_000_000))
        change(&m)
        return m
    }

    func testEachStateIsNamedPlainly() {
        XCTAssertEqual(describeOutbox(message()).state, "Queued")
        XCTAssertEqual(describeOutbox(message { $0.state = .sending }).state, "Sending…")
        XCTAssertEqual(describeOutbox(message { $0.state = .delivered }).state, "Delivered to the hub")
        XCTAssertEqual(describeOutbox(message { $0.state = .failed; $0.lastError = "Nope." }).reason, "Nope.")
    }

    func testAWaitNamesWhatTheLastTryMet() {
        let waiting = describeOutbox(
            message {
                $0.lastError = "x"
                $0.lastErrorKind = MessageErrorKind.unavailable.rawValue
                $0.failures = 2
            })
        XCTAssertEqual(waiting.state, "Queued, will retry")
        XCTAssertEqual(waiting.reason, "The hub can't be reached right now.")
        let off = describeOutbox(
            message {
                $0.lastError = "x"
                $0.lastErrorKind = MessageErrorKind.messagingOff.rawValue
            })
        XCTAssertTrue(try XCTUnwrap(off.reason).contains("sends as soon as it is turned on"))
    }

    func testAMessageThatMayHaveArrivedSaysResendingIsSafe() {
        let unsure = describeOutbox(message { $0.mayHaveSent = true })
        XCTAssertTrue(try XCTUnwrap(unsure.note).contains("same message id"))
        XCTAssertNil(describeOutbox(message()).note)
    }
}

// MARK: Through AppModel and the real client

/// Everything the operator relies on, end to end against the stub gateway: a
/// send while the hub is unreachable queues, the retries reuse one id, and the
/// hub sees one message.
@MainActor
final class OutboxDeliveryTests: XCTestCase {
    private let time = ManualClock()
    private let store = MemoryOutboxStore()
    private var path: String { managerPath(obProject, obPM) + "/messages" }

    override func setUp() {
        super.setUp()
        StubGateway.shared.reset()
        StubGateway.shared.script("GET /auth/bootstrap", .json(#"{"nonce":"n-1"}"#))
        StubGateway.shared.script("POST /auth/session", .json(#"{"csrf":"csrf-1"}"#), .json(#"{"csrf":"csrf-2"}"#))
    }

    private func makeModel(store: (any OutboxStore)? = nil, defaults: UserDefaults? = nil) throws -> AppModel {
        let suite = "rsi-outbox-tests-\(UUID().uuidString)"
        let defaults = try defaults ?? XCTUnwrap(UserDefaults(suiteName: suite))
        let ids = IDSource()
        let outbox = Outbox(
            store: store ?? self.store, clock: time.clock, jitter: { 0.5 }, newID: { ids.next() })
        let model = AppModel(defaults: defaults, outbox: outbox) { host in
            let configuration = URLSessionConfiguration.ephemeral
            // The launch fault's host never resolves: a real failure at the transport.
            configuration.protocolClasses = host.hasSuffix(".invalid") ? [DeadHostProtocol.self] : [StubProtocol.self]
            return RemoteClient(host: host, session: URLSession(configuration: configuration), retry: .instant)
        }
        model.setHost(obHost)
        model.now = { [time] in time.now }
        return model
    }

    private func makeChat(_ model: AppModel) throws -> ManagerChat {
        let list = try V2Fixture.named("projects_with_global_200").decode(ProjectSeatsList.self)
        model.applySeats(list)
        let client = try XCTUnwrap(model.client)
        return ManagerChat(
            project: obProject, seats: list.projects.first, client: client, host: obHost, outbox: model.outbox)
    }

    private func posts() -> [StubGateway.Seen] {
        StubGateway.shared.requests.filter { $0.method == "POST" && $0.path == path }
    }

    private func eventually(_ what: String, _ condition: () -> Bool, line: UInt = #line) async {
        for _ in 0..<600 {
            if condition() { return }
            try? await Task.sleep(for: .milliseconds(5))
        }
        XCTFail("timed out waiting for: \(what)", line: line)
    }

    func testAMessageSentWhileTheHostIsUnreachableIsDeliveredExactlyOnceWhenItReturns() async throws {
        let accepted = try V2Fixture.named("message_accepted_200").reply
        StubGateway.shared.script(
            "POST \(path)", .fail(.cannotConnectToHost), .fail(.notConnectedToInternet),
            .fail(.cannotConnectToHost), accepted)
        let model = try makeModel()
        let chat = try makeChat(model)
        chat.draft.text = "please prioritize the migration"
        let sent = try XCTUnwrap(chat.send())
        // The box is free at once and the message shows as queued, not lost.
        XCTAssertEqual(chat.draft.text, "")
        XCTAssertEqual(chat.outgoing.map(\.id), [sent.id])
        await eventually("first try fails") { model.outbox.message(sent.id)?.failures == 1 }
        XCTAssertEqual(model.outbox.message(sent.id)?.state, .queued)
        XCTAssertEqual(describeOutbox(try XCTUnwrap(model.outbox.message(sent.id))).state, "Queued, will retry")

        // The retries ride the schedule and reach the hub on the fourth try.
        for expected in 2...4 {
            await eventually("wait armed") { self.time.requested.count == expected - 1 }
            time.advance(try XCTUnwrap(time.requested.last))
            await eventually("try \(expected)") { self.posts().count == expected }
        }
        await eventually("delivered") { model.outbox.message(sent.id)?.state == .delivered }

        // One message, one id, byte-identical bodies on all four tries.
        let bodies = posts().map(\.body)
        XCTAssertEqual(bodies.count, 4)
        XCTAssertEqual(Set(bodies).count, 1)
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: bodies[0]) as? [String: Any])
        XCTAssertEqual(body["idempotency_key"] as? String, sent.id)
        XCTAssertEqual(body["text"] as? String, "please prioritize the migration")
        XCTAssertEqual(Set(posts().compactMap { $0.headers["x-rsi-csrf"] }).count, 1)
        // The conversation shows the hub's delivery state for it.
        XCTAssertEqual(chat.delivery(of: try XCTUnwrap(model.outbox.message(sent.id)))?.tone, .busy)
    }

    func testAnUnknownOutcomeIsRetriedUnderTheSameIdAndTheReplayIsNotADuplicate() async throws {
        StubGateway.shared.script(
            "POST \(path)", try V2Fixture.named("message_outcome_unknown_504").reply,
            .fail(.networkConnectionLost), try V2Fixture.named("message_replayed_200").reply)
        let model = try makeModel()
        let chat = try makeChat(model)
        chat.draft.text = "ship it"
        let sent = try XCTUnwrap(chat.send())
        for expected in 2...3 {
            await eventually("wait armed") { self.time.requested.count == expected - 1 }
            time.advance(try XCTUnwrap(time.requested.last))
            await eventually("try \(expected)") { self.posts().count == expected }
        }
        await eventually("delivered") { model.outbox.message(sent.id)?.state == .delivered }
        let done = try XCTUnwrap(model.outbox.message(sent.id))
        XCTAssertTrue(done.mayHaveSent)
        XCTAssertTrue(done.deduplicated)
        XCTAssertEqual(Set(posts().map(\.body)).count, 1)
        XCTAssertEqual(posts().count, 3)
        XCTAssertTrue(try XCTUnwrap(chat.delivery(of: done)).text.hasPrefix("Already queued earlier"))
    }

    func testTheOutboxSurvivesKillingTheAppAndDeliversOnReopen() async throws {
        StubGateway.shared.script("POST \(path)", .fail(.cannotConnectToHost))
        var model = try makeModel()
        var chat = try makeChat(model)
        chat.draft.text = "do not lose me"
        let sent = try XCTUnwrap(chat.send())
        await eventually("first try fails") { model.outbox.message(sent.id)?.failures == 1 }
        XCTAssertEqual(posts().count, 1)

        // Kill the app: drop every object, keep only what is on disk.
        let defaults = UserDefaults(suiteName: "rsi-outbox-reopen-\(UUID().uuidString)")
        model = try makeModel(defaults: defaults)
        chat = try makeChat(model)
        let revived = try XCTUnwrap(chat.outgoing.first)
        XCTAssertEqual(revived.id, sent.id)
        XCTAssertEqual(revived.text, "do not lose me")
        XCTAssertEqual(chat.outgoing.count, 1)

        // The host is back; the reopened app delivers it with no action from the operator.
        StubGateway.shared.script("POST \(path)", try V2Fixture.named("message_accepted_200").reply)
        time.advance(2)
        model.outbox.kick()
        await eventually("delivered") { model.outbox.message(sent.id)?.state == .delivered }
        let last = try XCTUnwrap(posts().last)
        XCTAssertEqual(
            (try JSONSerialization.jsonObject(with: last.body) as? [String: Any])?["idempotency_key"] as? String,
            sent.id)
    }

    func testAMessagingOffHubHoldsTheMessageAndDeliversWhenItIsEnabled() async throws {
        StubGateway.shared.script("POST \(path)", try V2Fixture.named("message_switch_off_403").reply)
        StubGateway.shared.script("GET /api/v2/projects", try V2Fixture.named("projects_with_global_200").reply)
        let model = try makeModel()
        let chat = try makeChat(model)
        chat.draft.text = "ship it"
        let sent = try XCTUnwrap(chat.send())
        await eventually("held") {
            model.outbox.message(sent.id)?.lastErrorKind == MessageErrorKind.messagingOff.rawValue
        }
        XCTAssertEqual(model.outbox.message(sent.id)?.state, .queued)
        XCTAssertTrue(chat.messagingOff)
        XCTAssertEqual(posts().count, 1)

        // The operator turns messaging on; the next list says so and the message goes out.
        StubGateway.shared.script("POST \(path)", try V2Fixture.named("message_accepted_200").reply)
        model.applySeats(ProjectSeatsList(projects: [], messagesEnabled: true))
        await eventually("delivered") { model.outbox.message(sent.id)?.state == .delivered }
        XCTAssertFalse(chat.messagingOff)
        XCTAssertEqual(Set(posts().map(\.body)).count, 1)
    }

    func testTheLaunchFaultTakesSendsToAnUnreachableHostWithoutChangingTheSavedHost() async throws {
        StubGateway.shared.script("POST \(path)", try V2Fixture.named("message_accepted_200").reply)
        let model = try makeModel()
        model.simulateUnreachableSends(until: time.now.addingTimeInterval(20))
        let chat = try makeChat(model)
        chat.draft.text = "queued by the fault"
        let sent = try XCTUnwrap(chat.send())
        // The fault client is a real client for an unresolvable host: nothing reaches the stub.
        await eventually("held by the fault") { model.outbox.message(sent.id)?.failures == 1 }
        XCTAssertTrue(posts().isEmpty)
        XCTAssertEqual(model.host, obHost)
        XCTAssertEqual(model.outbox.message(sent.id)?.lastErrorKind, MessageErrorKind.unavailable.rawValue)
        time.advance(21)
        await eventually("delivered once the fault window ends") { model.outbox.message(sent.id)?.state == .delivered }
        XCTAssertEqual(posts().count, 1)
    }
}

/// A host that never answers: every request fails as an unresolvable name would.
final class DeadHostProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() { client?.urlProtocol(self, didFailWithError: URLError(.cannotFindHost)) }
    override func stopLoading() {}
}

// MARK: Launch arguments

@MainActor
final class OutboxLaunchTests: XCTestCase {
    private func defaults(_ pairs: [String: String]) throws -> UserDefaults {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: "rsi-outbox-launch-\(UUID().uuidString)"))
        for (key, value) in pairs { defaults.set(value, forKey: key) }
        return defaults
    }

    func testTheFaultSecondsAreReadAndBounded() throws {
        XCTAssertNil(OutboxLaunch.faultSeconds(try defaults([:])))
        XCTAssertNil(OutboxLaunch.faultSeconds(try defaults(["outboxFaultSeconds": "soon"])))
        XCTAssertNil(OutboxLaunch.faultSeconds(try defaults(["outboxFaultSeconds": "-4"])))
        XCTAssertEqual(OutboxLaunch.faultSeconds(try defaults(["outboxFaultSeconds": "25"])), 25)
        XCTAssertEqual(OutboxLaunch.faultSeconds(try defaults(["outboxFaultSeconds": "99999"])), 600)
    }

    func testTheProbeQueuesOneMessageForTheFirstManagerAndNotAgainOnRelaunch() async throws {
        let list = try V2Fixture.named("projects_with_global_200").decode(ProjectSeatsList.self)
        let defaults = try defaults(["outboxProbe": "probe from launch"])
        let store = MemoryOutboxStore()
        let transport = ScriptedTransport()
        func launch() -> AppModel {
            let outbox = Outbox(store: store)
            let model = AppModel(defaults: defaults, outbox: outbox)
            model.setHost(obHost)
            model.projects = [
                ProjectRow(project: WireProject(id: obProject, name: "Alpha"), waiting: 0, sessions: [], seats: list.projects.first)
            ]
            return model
        }
        let first = launch()
        first.outbox.transport = transport
        transport.activeHost = nil  // keep the probe queued: this test is about what is saved
        await first.start()
        let saved = store.load().entries
        XCTAssertEqual(saved.map(\.text), ["probe from launch"])
        XCTAssertEqual(saved.first?.seat, list.projects.first?.projectManager?.sessionId)
        XCTAssertEqual(saved.first?.host, obHost)
        let second = launch()
        await second.start()
        XCTAssertEqual(second.outbox.entries.count, 1)
        XCTAssertEqual(second.host, obHost)
    }
}
