import Foundation
import XCTest

@testable import RSIRemote

// Adoption of hub #3797 (#515): exact tool-call pairing by call id, tool rows
// in the manager chat, the outbox bubble matched to its delivered item, and
// session history paged with the served next_cursor. The JSON below is the wire
// shape of the hub change (crates/rsi-common remote_manager_v2.rs,
// docs/rsi-remote/contracts-v2.md; V1 history from contracts/v1/fixtures);
// every case also runs against the older shape without the new fields.

// MARK: Fixtures

private func historyEvent(
    _ id: Int, kind: String, role: String? = nil, text: String = "t", tool: String? = nil,
    key: String? = nil, pairing: String? = nil
) throws -> HistoryEvent {
    var fields: [String] = [
        #""id":"\#(id)""#, #""sequence":\#(id)"#,
        #""kind":{"state":"known","value":"\#(kind)"}"#,
        role.map { #""role":{"state":"known","value":"\#($0)"}"# } ?? #""role":null"#,
        #""created_at":"2026-10-22T10:00:\#(String(format: "%02d", id % 60)).000000000Z""#,
        #""text":"\#(text)""#, #""content_bytes":"\#(text.utf8.count)""#, #""truncated":false"#,
        #""tool_name":\#(tool.map { "\"\($0)\"" } ?? "null")"#,
    ]
    if let key { fields.append(#""tool_pair_key":"\#(key)","tool_id_display":"\#(key)""#) }
    if let pairing { fields.append(#""pairing_state":"\#(pairing)""#) }
    return try JSONDecoder().decode(HistoryEvent.self, from: Data("{\(fields.joined(separator: ","))}".utf8))
}

private func historyUse(_ id: Int, _ tool: String, key: String? = nil, pairing: String? = nil) throws -> HistoryEvent {
    try historyEvent(id, kind: "ToolUse", text: "in\(id)", tool: tool, key: key, pairing: pairing)
}

private func historyResult(_ id: Int, key: String? = nil, pairing: String? = nil) throws -> HistoryEvent {
    try historyEvent(id, kind: "ToolResult", text: "out\(id)", tool: nil, key: key, pairing: pairing)
}

private func calls(_ events: [HistoryEvent]) -> [ToolCall] {
    ChatTranscript.build(history: events).flatMap(\.calls)
}

// MARK: Exact pairing by call id (session history)

final class ToolCallIdPairingTests: XCTestCase {
    func testTheRealV1CorpusPairsUseAndResultByTheirSharedKey() throws {
        let bundle = Bundle(for: ToolCallIdPairingTests.self)
        let url = try XCTUnwrap(bundle.url(forResource: "corpus", withExtension: "json"))
        let corpus = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let cases = try XCTUnwrap(corpus["cases"] as? [[String: Any]])
        let entry = try XCTUnwrap(cases.first { $0["id"] as? String == "history-tool-pair-display-separation" })
        let input = try XCTUnwrap(entry["input"] as? [String: Any])
        let value = try XCTUnwrap(input["value"] as? [String: Any])
        let result = try JSONSerialization.data(withJSONObject: try XCTUnwrap(value["result"]))
        let page = try JSONDecoder().decode(HistoryPageResult.self, from: result)
        let exact = page.items.filter { $0.toolPairKey == "exact-call-id" }
        XCTAssertEqual(exact.map(\.pairingState), ["exact", "exact"])
        let call = try XCTUnwrap(calls(page.items).first { $0.callId == "exact-call-id" })
        XCTAssertNotNil(call.use)
        XCTAssertNotNil(call.result)
        XCTAssertFalse(call.running)
    }

    func testParallelCallsWhoseResultsReturnOutOfOrderPairExactly() throws {
        // A and B start together; B's result comes back first. Oldest-first
        // pairing would hand B's output to A.
        let events = [
            try historyUse(1, "Bash", key: "toolu_A", pairing: "exact"),
            try historyUse(2, "Read", key: "toolu_B", pairing: "exact"),
            try historyResult(3, key: "toolu_B", pairing: "exact"),
            try historyResult(4, key: "toolu_A", pairing: "exact"),
        ]
        let paired = calls(events)
        XCTAssertEqual(paired.count, 2)
        XCTAssertEqual(paired[0].use?.id, "1")
        XCTAssertEqual(paired[0].result?.id, "4", "A gets A's result")
        XCTAssertEqual(paired[1].use?.id, "2")
        XCTAssertEqual(paired[1].result?.id, "3", "B gets B's result")
        XCTAssertFalse(paired.contains { $0.running })

        let guessed = calls(try events.map { event in
            try historyEvent(
                Int(event.id) ?? 0, kind: event.kind.knownValue ?? "", text: event.text, tool: event.toolName)
        })
        XCTAssertEqual(guessed[0].result?.id, "3", "without ids the old guess pairs A with B's result")
    }

    func testAResultFindsItsCallAcrossAnInterveningMessage() throws {
        let events = [
            try historyUse(1, "Bash", key: "toolu_A"),
            try historyEvent(2, kind: "Message", role: "Assistant", text: "working"),
            try historyResult(3, key: "toolu_A"),
        ]
        let paired = calls(events)
        XCTAssertEqual(paired.count, 1)
        XCTAssertEqual(paired[0].result?.id, "3")
    }

    func testAResultWhoseCallIsOnAnOlderPageStandsAloneAndTakesNoOtherCall() throws {
        let events = [
            try historyUse(5, "Bash", key: "toolu_here"),
            try historyResult(6, key: "toolu_gone"),
        ]
        let paired = calls(events)
        XCTAssertEqual(paired.count, 2)
        XCTAssertTrue(paired[0].running, "the call still waits for its own result")
        XCTAssertNil(paired[1].use)
        XCTAssertEqual(paired[1].result?.id, "6")
    }

    func testACallStillRunningHasNoResultYet() throws {
        let paired = calls([try historyUse(1, "Bash", key: "toolu_A", pairing: "exact")])
        XCTAssertEqual(paired.count, 1)
        XCTAssertTrue(paired[0].running)
    }

    func testAnAmbiguousKeyStillPairsOldestFirstWithinThatKey() throws {
        // The hub marks a key it saw on more than one row `ambiguous`; the pair
        // is then the oldest waiting call with that key, never a different key.
        let events = [
            try historyUse(1, "Bash", key: "toolu_X", pairing: "ambiguous"),
            try historyUse(2, "Bash", key: "toolu_X", pairing: "ambiguous"),
            try historyUse(3, "Read", key: "toolu_Y", pairing: "exact"),
            try historyResult(4, key: "toolu_X", pairing: "ambiguous"),
        ]
        let paired = calls(events)
        XCTAssertEqual(paired.map { $0.result?.id }, ["4", nil, nil])
        XCTAssertTrue(paired[2].running, "toolu_Y is not touched by a toolu_X result")
    }

    func testMissingAndOversizedIdsFallBackToOldestFirst() throws {
        // `missing` and `oversized` carry no usable key, exactly as an older hub.
        let events = [
            try historyUse(1, "Bash", pairing: "oversized"),
            try historyUse(2, "Read", pairing: "missing"),
            try historyResult(3, pairing: "oversized"),
        ]
        let paired = calls(events)
        XCTAssertEqual(paired[0].result?.id, "3")
        XCTAssertTrue(paired[1].running)
    }

    func testAnOlderHubWithoutTheFieldsDecodesAndPairsAsBefore() throws {
        let json = #"""
            {"id":"7","sequence":7,"kind":{"state":"known","value":"ToolUse"},"role":null,
             "created_at":"2026-10-22T10:00:07Z","text":"x","content_bytes":"1","truncated":false,
             "tool_name":"Bash"}
            """#
        let event = try JSONDecoder().decode(HistoryEvent.self, from: Data(json.utf8))
        XCTAssertNil(event.toolPairKey)
        XCTAssertNil(event.pairingState)
        let paired = calls([
            try historyUse(1, "Bash"), try historyUse(2, "Read"), try historyResult(3), try historyResult(4),
        ])
        XCTAssertEqual(paired.map { $0.result?.id }, ["3", "4"])
    }

    func testAPlainMessageNeverCarriesACallId() throws {
        let event = ChatEvent(
            try historyEvent(1, kind: "Message", role: "Assistant", key: "toolu_stray"))
        XCTAssertNil(event.callId)
    }
}

// MARK: Tool rows in the manager chat

private func conversationItem(_ json: String) throws -> ConversationItem {
    try JSONDecoder().decode(ConversationItem.self, from: Data(json.utf8))
}

private func toolItemJSON(
    _ id: Int, _ kind: String, name: String? = "Bash", callId: String? = nil, text: String = "t"
) -> String {
    var fields = [
        #""id":"\#(id)""#, #""sequence":\#(id)"#, #""speaker":"manager""#, #""text":"\#(text)""#,
        #""truncated":false"#, #""created_at":"2026-10-22T10:00:\#(String(format: "%02d", id % 60)).000000000Z""#,
        #""operator_message_id":null"#, #""kind":"\#(kind)""#,
    ]
    if let name { fields.append(#""tool_name":"\#(name)""#) }
    if let callId { fields.append(#""tool_use_id":"\#(callId)""#) }
    return "{\(fields.joined(separator: ","))}"
}

private func toolItem(
    _ id: Int, _ kind: String, name: String? = "Bash", callId: String? = nil, text: String = "t"
) throws -> ConversationItem {
    try conversationItem(toolItemJSON(id, kind, name: name, callId: callId, text: text))
}

private func messageItemJSON(_ id: Int, _ speaker: String, _ text: String = "m") -> String {
    #"""
    {"id":"\#(id)","sequence":\#(id),"speaker":"\#(speaker)","text":"\#(text)","truncated":false,
     "created_at":"2026-10-22T10:00:\#(String(format: "%02d", id % 60)).000000000Z",
     "operator_message_id":null}
    """#
}

private func messageItem(_ id: Int, _ speaker: String, _ text: String = "m") throws -> ConversationItem {
    try conversationItem(messageItemJSON(id, speaker, text))
}

private func conversationPage(_ items: [String], next: String?, hasMore: Bool = true) throws -> ConversationPage {
    let json = #"""
        {"seat":{"session_id":"s","role":"project_manager","status":"Running","provider":"Claude",
          "model":null,"last_activity_at":"2026-10-22T10:00:00Z","pending_decisions":0,
          "pending_decisions_truncated":false},
         "items":[\#(items.joined(separator: ","))],
         "next_before":\#(next.map { "\"\($0)\"" } ?? "null"),"has_more":\#(hasMore),"operator_messages":[]}
        """#
    return try JSONDecoder().decode(ConversationPage.self, from: Data(json.utf8))
}
final class ManagerChatToolRowTests: XCTestCase {
    func testAToolItemFoldsIntoACollapsedToolRowOfTheManagersTurn() throws {
        let items = [
            try messageItem(1, "operator", "status?"),
            try toolItem(2, "tool_use", callId: "toolu_1", text: "git status"),
            try toolItem(3, "tool_result", name: nil, callId: "toolu_1", text: "clean"),
            try messageItem(4, "manager", "All clean."),
        ]
        let turns = ChatTranscript.build(items: items)
        XCTAssertEqual(turns.map(\.role), [.user, .assistant])
        XCTAssertEqual(turns[1].parts.count, 2, "one folded tool row, then the reply")
        guard case .tools(_, let folded) = turns[1].parts[0], case .text = turns[1].parts[1] else {
            return XCTFail("parts: \(turns[1].parts)")
        }
        XCTAssertEqual(folded.count, 1)
        XCTAssertEqual(folded[0].name, "Bash")
        XCTAssertEqual(folded[0].callId, "toolu_1")
        XCTAssertEqual(folded[0].use?.text, "git status")
        XCTAssertEqual(folded[0].result?.text, "clean")
        XCTAssertEqual(toolSummary(folded), "Used Bash")
        let disclosure = Disclosure()
        XCTAssertFalse(disclosure.isOpen(turns[1].parts[0].id), "starts collapsed")
        XCTAssertEqual(turns[1].eventCount, 3)
    }

    func testParallelManagerCallsPairByTheirIds() throws {
        let items = [
            try toolItem(1, "tool_use", name: "Bash", callId: "a"),
            try toolItem(2, "tool_use", name: "Read", callId: "b"),
            try toolItem(3, "tool_result", name: "Read", callId: "b", text: "read out"),
            try toolItem(4, "tool_result", name: "Bash", callId: "a", text: "bash out"),
        ]
        let paired = ChatTranscript.build(items: items).flatMap(\.calls)
        XCTAssertEqual(paired.map { $0.result?.text }, ["bash out", "read out"])
    }

    func testAnItemWithoutAKindIsAMessageAsOnAnOlderHub() throws {
        let old = try messageItem(9, "manager", "hello")
        XCTAssertNil(old.kind)
        XCTAssertNil(old.toolPhase)
        XCTAssertNil(old.idempotencyKey)
        let turns = ChatTranscript.build(items: [old])
        XCTAssertEqual(turns.count, 1)
        guard case .text(let event) = turns[0].parts[0] else { return XCTFail("parts") }
        XCTAssertEqual(event.role, .assistant)
        XCTAssertEqual(event.text, "hello")
    }

    func testAToolItemWithoutAnIdPairsOldestFirst() throws {
        // The hub drops a call id over 256 bytes: both halves arrive without one.
        let items = [
            try toolItem(1, "tool_use", callId: nil),
            try toolItem(2, "tool_result", name: nil, callId: nil, text: "done"),
        ]
        let paired = ChatTranscript.build(items: items).flatMap(\.calls)
        XCTAssertEqual(paired.count, 1)
        XCTAssertEqual(paired[0].result?.text, "done")
    }

    func testAToolItemMidThreadMergesAcrossPagesLikeAnyItem() throws {
        var thread = SeatThread(seat: "s")
        thread.mergeFirstPage(
            try conversationPage(
                [toolItemJSON(4, "tool_result", callId: "x"), messageItemJSON(3, "manager")], next: "3:3"))
        thread.appendOlderPage(
            try conversationPage([toolItemJSON(2, "tool_use", callId: "x")], next: nil, hasMore: false))
        let paired = ChatTranscript.build(items: thread.items).flatMap(\.calls)
        XCTAssertEqual(paired.count, 1)
        XCTAssertNotNil(paired[0].use)
        XCTAssertNotNil(paired[0].result)
    }
}

// MARK: The outbox bubble clears on delivery

final class OutboxDeliveryMatchTests: XCTestCase {
    private let key = "11111111-1111-4111-8111-111111111111"
    private let hubID = "550e8400-e29b-41d4-a716-44665544000a"

    private func delivered(hub: String? = nil) -> OutboxMessage {
        var message = OutboxMessage(
            id: key, host: "h", project: "p", role: "project_manager", seat: "s", text: "hello",
            createdAt: Date(timeIntervalSince1970: 10))
        message.state = .delivered
        message.hubMessageId = hub ?? hubID
        message.hubState = "queued"
        return message
    }

    private func item(operatorMessageId: String?, key: String?, speaker: String = "operator") throws -> ConversationItem {
        let id = operatorMessageId.map { "\"\($0)\"" } ?? "null"
        let echoed = key.map { #","idempotency_key":"\#($0)""# } ?? ""
        return try conversationItem(
            #"""
            {"id":"30","sequence":30,"speaker":"\#(speaker)","text":"hello","truncated":false,
             "created_at":"2026-10-22T10:00:00Z","operator_message_id":\#(id)\#(echoed)}
            """#)
    }

    private func record(state: String, key: String?, id: String? = nil) throws -> OperatorMessageState {
        let echoed = key.map { #","idempotency_key":"\#($0)""# } ?? ""
        return try JSONDecoder().decode(
            OperatorMessageState.self,
            from: Data(
                #"""
                {"message_id":"\#(id ?? hubID)","state":"\#(state)","created_at":"2026-10-22T10:00:00Z",
                 "updated_at":"2026-10-22T10:00:05Z","delivered_at":null,"text_len":5\#(echoed)}
                """#.utf8))
    }

    func testTheItemThatEchoesTheKeyTheDeviceSentTakesTheBubbleOver() throws {
        let pending = pendingOutgoing(
            [delivered()], conversation: [try item(operatorMessageId: hubID, key: key)])
        XCTAssertEqual(pending.map(\.id), [])
    }

    func testTheKeyAloneMatchesEvenWhenTheItemCarriesNoHubId() throws {
        let pending = pendingOutgoing([delivered()], conversation: [try item(operatorMessageId: nil, key: key.uppercased())])
        XCTAssertEqual(pending.map(\.id), [], "the key compares without regard to case")
    }

    func testADeliveredRecordClearsTheBubbleWhenTheItemShowsAsASystemNote() throws {
        // The #505 probe on the live hub: a system-style item without an id.
        let system = try item(operatorMessageId: nil, key: nil, speaker: "system")
        XCTAssertEqual(pendingOutgoing([delivered()], conversation: [system]).map(\.id), [key], "no delivery record yet")
        let records = [try record(state: "delivered", key: key)]
        XCTAssertEqual(pendingOutgoing([delivered()], conversation: [system], deliveries: records).map(\.id), [])
    }

    func testTheRecordMatchesByHubIdOnAnOlderHubThatDoesNotEchoTheKey() throws {
        let records = [try record(state: "delivered", key: nil)]
        XCTAssertEqual(pendingOutgoing([delivered()], conversation: [], deliveries: records).map(\.id), [])
    }

    func testAMessageTheHubHasOnlyQueuedStaysAsABubble() throws {
        let records = [try record(state: "queued", key: key)]
        XCTAssertEqual(pendingOutgoing([delivered()], conversation: [], deliveries: records).map(\.id), [key])
    }

    func testADifferentMessagesRecordOrItemDoesNotClearIt() throws {
        let other = "22222222-2222-4222-8222-222222222222"
        let records = [try record(state: "delivered", key: other, id: "someone-else")]
        let items = [try item(operatorMessageId: "someone-else", key: other)]
        XCTAssertEqual(pendingOutgoing([delivered()], conversation: items, deliveries: records).map(\.id), [key])
    }

    func testAnUnsentMessageStaysEvenWhenItsKeyIsAlreadyOnAnItem() throws {
        var queued = delivered()
        queued.state = .queued
        queued.hubMessageId = nil
        let items = [try item(operatorMessageId: hubID, key: key)]
        XCTAssertEqual(pendingOutgoing([queued], conversation: items).map(\.id), [key])
    }

    func testTheDeliveryLineFollowsTheRecordByKey() throws {
        var thread = SeatThread(seat: "s")
        let echoed = try record(state: "delivered", key: key, id: "other-id")
        thread.operatorMessages = [echoed]
        let event = ChatEvent(try item(operatorMessageId: nil, key: key))
        XCTAssertEqual(thread.deliveryState(of: event), "delivered")
        let unrelated = ChatEvent(try item(operatorMessageId: nil, key: nil))
        XCTAssertNil(thread.deliveryState(of: unrelated))
    }

    func testTheWireShapeOfTheNewFieldsDecodesAndTheOldShapeStillDoes() throws {
        let old = try record(state: "delivered", key: nil)
        XCTAssertNil(old.idempotencyKey)
        let now = try record(state: "delivered", key: key)
        XCTAssertEqual(now.idempotencyKey, key)
        XCTAssertTrue(now.describes(delivered()))
        XCTAssertTrue(old.describes(delivered()), "falls back to the hub id")
        let stranger = try record(state: "delivered", key: "22222222-2222-4222-8222-222222222222", id: "other")
        XCTAssertFalse(stranger.describes(delivered()))
    }
}

// MARK: Session history, Load older

private func historyPage(_ events: [HistoryEvent], next: String?) throws -> HistoryPageResult {
    let items = try events.map { event -> String in
        let key = event.toolPairKey.map { #","tool_pair_key":"\#($0)","tool_id_display":"\#($0)""# } ?? ""
        return #"""
            {"id":"\#(event.id)","sequence":\#(event.sequence),"kind":{"state":"known","value":"\#(event.kind.knownValue ?? "Message")"},
             "role":null,"created_at":"\#(event.createdAt)","text":"\#(event.text)","content_bytes":"1",
             "truncated":false,"tool_name":null\#(key)}
            """#
    }
    let cursor = next.map { #"{"kind":"history","token":"\#($0)"}"# } ?? "null"
    let json = #"{"complete":true,"next_cursor":\#(cursor),"items":[\#(items.joined(separator: ","))]}"#
    return try JSONDecoder().decode(HistoryPageResult.self, from: Data(json.utf8))
}

private func messages(_ range: ClosedRange<Int>) throws -> [HistoryEvent] {
    try range.map { try historyEvent($0, kind: "Message", role: "Assistant", text: "e\($0)") }
}

final class HistoryThreadTests: XCTestCase {
    func testTheFirstPageSetsTheCursorAndLoadOlderPrependsThePageBefore() throws {
        var thread = HistoryThread()
        XCTAssertFalse(thread.canLoadOlder)
        thread.mergeFirstPage(try historyPage(try messages(26...50), next: "tok-1"))
        XCTAssertTrue(thread.canLoadOlder)
        XCTAssertEqual(thread.events.count, 25)
        thread.loadingOlder = true
        thread.appendOlderPage(try historyPage(try messages(1...25), next: nil), cursor: "tok-1")
        XCTAssertEqual(thread.events.map(\.id), (1...50).map(String.init), "oldest first, none repeated")
        XCTAssertFalse(thread.canLoadOlder, "the oldest page is in")
        XCTAssertFalse(thread.loadingOlder)
    }

    func testAPollKeepsTheOlderPagesAndTheCursorAndAddsTheNewEvents() throws {
        var thread = HistoryThread()
        thread.mergeFirstPage(try historyPage(try messages(26...50), next: "tok-1"))
        thread.appendOlderPage(try historyPage(try messages(1...25), next: "tok-0"), cursor: "tok-1")
        thread.mergeFirstPage(try historyPage(try messages(30...53), next: "tok-9"))
        XCTAssertEqual(thread.events.map(\.id), (1...53).map(String.init))
        XCTAssertEqual(thread.olderCursor, "tok-0", "a poll never moves the cursor forward")
    }

    func testAPollBeforeAnyOlderPageKeepsTheFirstCursorToo() throws {
        var thread = HistoryThread()
        thread.mergeFirstPage(try historyPage(try messages(26...50), next: "tok-1"))
        thread.mergeFirstPage(try historyPage(try messages(28...52), next: "tok-2"))
        XCTAssertEqual(thread.olderCursor, "tok-1", "events older than 26 are still behind the first cursor")
        XCTAssertEqual(thread.events.count, 27)
    }

    func testAPollWithNoOverlapReplacesTheThreadRatherThanLeaveAGap() throws {
        var thread = HistoryThread()
        thread.mergeFirstPage(try historyPage(try messages(1...25), next: "tok-1"))
        thread.mergeFirstPage(try historyPage(try messages(80...104), next: "tok-5"))
        XCTAssertEqual(thread.events.first?.id, "80")
        XCTAssertEqual(thread.events.count, 25)
        XCTAssertEqual(thread.olderCursor, "tok-5")
    }

    func testAFreshPollReplacesAnEventWithItsNewestForm() throws {
        var thread = HistoryThread()
        thread.mergeFirstPage(try historyPage([try historyUse(1, "Bash", key: "k")], next: nil))
        thread.mergeFirstPage(
            try historyPage([try historyUse(1, "Bash", key: "k"), try historyResult(2, key: "k")], next: nil))
        XCTAssertEqual(calls(thread.events).count, 1)
        XCTAssertNotNil(calls(thread.events).first?.result, "the result that arrived pairs with the call already shown")
    }

    func testALostConnectionKeepsTheCursorForARetry() throws {
        var thread = HistoryThread()
        thread.mergeFirstPage(try historyPage(try messages(26...50), next: "tok-1"))
        thread.loadingOlder = true
        let reload = thread.olderFailed(.unreachable("offline"), cursor: "tok-1")
        XCTAssertFalse(reload)
        XCTAssertEqual(thread.olderCursor, "tok-1")
        XCTAssertEqual(thread.olderError, .unreachable("offline"))
        XCTAssertFalse(thread.loadingOlder)
        thread.appendOlderPage(try historyPage(try messages(1...25), next: nil), cursor: "tok-1")
        XCTAssertNil(thread.olderError, "a good page clears the error")
    }

    func testARefusedCursorStartsTheThreadOverFromTheNewestPage() throws {
        var thread = HistoryThread()
        thread.mergeFirstPage(try historyPage(try messages(26...50), next: "tok-1"))
        let reload = thread.olderFailed(.server(status: 400, code: "stale_cursor"), cursor: "tok-1")
        XCTAssertTrue(reload, "the screen reads the newest page again")
        thread.mergeFirstPage(try historyPage(try messages(30...54), next: "tok-7"))
        XCTAssertEqual(thread.events.first?.id, "30")
        XCTAssertEqual(thread.olderCursor, "tok-7")
        XCTAssertNil(thread.olderError)
    }

    func testAnOlderPageForACursorThatIsNoLongerCurrentIsDropped() throws {
        var thread = HistoryThread()
        thread.mergeFirstPage(try historyPage(try messages(26...50), next: "tok-1"))
        thread.mergeFirstPage(try historyPage(try messages(90...114), next: "tok-5"))
        thread.appendOlderPage(try historyPage(try messages(1...25), next: nil), cursor: "tok-1")
        XCTAssertEqual(thread.events.first?.id, "90")
        XCTAssertEqual(thread.olderCursor, "tok-5")
    }

    func testAPageWithNoNextCursorLeavesNothingToLoad() throws {
        var thread = HistoryThread()
        thread.mergeFirstPage(try historyPage(try messages(1...3), next: nil))
        XCTAssertFalse(thread.canLoadOlder)
    }
}

// MARK: The client asks for the older page with the served cursor

final class HistoryCursorRequestTests: XCTestCase {
    func testTheClientSendsTheServedCursorAndTheNewestPageSendsNone() async throws {
        StubGateway.shared.reset()
        StubGateway.shared.script("GET /auth/bootstrap", .json(#"{"nonce":"n-1"}"#))
        StubGateway.shared.script("POST /auth/session", .json(#"{"csrf":"csrf-1"}"#))
        StubGateway.shared.script(
            "GET /api/v1/projects/p-1/sessions/s-1/history",
            .json(#"{"complete":true,"next_cursor":{"kind":"history","token":"tok_A-b"},"items":[]}"#))
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubProtocol.self]
        let client = RemoteClient(
            host: "hub.example.ts.net", session: URLSession(configuration: configuration), retry: .instant)
        let newest = try await client.history(project: "p-1", session: "s-1")
        XCTAssertEqual(newest.nextCursor?.token, "tok_A-b")
        _ = try await client.history(project: "p-1", session: "s-1", cursor: try XCTUnwrap(newest.nextCursor?.token))
        let sent = StubGateway.shared.requests.filter { $0.path == "/api/v1/projects/p-1/sessions/s-1/history" }
        XCTAssertEqual(sent.map(\.query), [nil, "cursor=tok_A-b"])
    }
}
