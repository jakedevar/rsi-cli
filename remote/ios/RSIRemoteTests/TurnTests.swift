import Foundation
import XCTest

@testable import RSIRemote

// The conversation as a chat app shows it (#502): role grouping, tool-call
// folding (collapsed by default, expandable), streaming append, the tail
// follower, empty and very long sessions, pending outbox bubbles and the light
// variant of a theme. All pure; no view is built.

private func event(
    _ id: Int, _ role: ChatRole, _ text: String = "t", tool: String? = nil, phase: ToolPhase? = nil,
    operatorMessageId: String? = nil
) -> ChatEvent {
    ChatEvent(
        id: String(id), sequence: id, role: role, text: text,
        createdAt: "2026-10-22T10:00:\(String(format: "%02d", id % 60))Z", toolName: tool, phase: phase,
        operatorMessageId: operatorMessageId)
}

private func use(_ id: Int, _ name: String, _ input: String = "{}") -> ChatEvent {
    event(id, .tool, input, tool: name, phase: .use)
}

private func result(_ id: Int, _ name: String? = nil, _ output: String = "ok") -> ChatEvent {
    event(id, .tool, output, tool: name, phase: .result)
}

// MARK: Role grouping

final class TurnGroupingTests: XCTestCase {
    func testAnEmptySessionHasNoTurns() {
        XCTAssertEqual(ChatTranscript.build([]), [])
        let newest = ChatTranscript.newest([])
        XCTAssertNil(newest.id)
        XCTAssertEqual(newest.count, 0)
    }

    func testTurnsFollowTheSpeaker() {
        let turns = ChatTranscript.build([
            event(1, .user, "hi"), event(2, .assistant, "hello"), event(3, .assistant, "more"),
            event(4, .user, "again"), event(5, .assistant, "ok"),
        ])
        XCTAssertEqual(turns.map(\.role), [.user, .assistant, .user, .assistant])
        XCTAssertEqual(turns.map(\.id), ["1", "2", "4", "5"])
        XCTAssertEqual(turns[1].parts.count, 2, "two assistant messages in a row are one turn")
        XCTAssertEqual(turns.map(\.eventCount), [1, 2, 1, 1])
    }

    func testConsecutiveUserMessagesAreOneTurnOfStackedBubbles() {
        let turns = ChatTranscript.build([event(1, .user, "a"), event(2, .user, "b")])
        XCTAssertEqual(turns.count, 1)
        XCTAssertEqual(turns[0].parts.count, 2)
    }

    func testNewestFirstInputIsReadOldestFirst() {
        // The hub pages newest first.
        let turns = ChatTranscript.build([
            event(4, .assistant, "d"), event(3, .user, "c"), event(2, .assistant, "b"), event(1, .user, "a"),
        ])
        XCTAssertEqual(turns.map(\.role), [.user, .assistant, .user, .assistant])
        XCTAssertEqual(turns.map(\.id), ["1", "2", "3", "4"])
    }

    func testIdsOrderNumericallyNotAsText() {
        let a = event(9, .user)
        let b = ChatEvent(id: "10", sequence: 9, role: .assistant, text: "", createdAt: "")
        XCTAssertEqual(ChatTranscript.build([b, a]).map(\.id), ["9", "10"])
    }

    func testADuplicateEventAppearsOnce() {
        let turns = ChatTranscript.build([event(1, .user), event(1, .user), event(2, .assistant)])
        XCTAssertEqual(turns.map(\.eventCount), [1, 1])
    }

    func testSystemNotesStandApartFromBothSpeakers() {
        let turns = ChatTranscript.build([event(1, .user), event(2, .system, "rotated"), event(3, .assistant)])
        XCTAssertEqual(turns.map(\.role), [.user, .system, .assistant])
    }

    func testToolAndThinkingEventsBelongToTheAssistantTurn() {
        let turns = ChatTranscript.build([
            event(1, .user), event(2, .thinking, "hm"), use(3, "Bash"), result(4, "Bash"), event(5, .assistant, "done"),
        ])
        XCTAssertEqual(turns.map(\.role), [.user, .assistant])
        guard case .thinking = turns[1].parts[0], case .tools = turns[1].parts[1], case .text = turns[1].parts[2]
        else { return XCTFail("parts: \(turns[1].parts)") }
    }

    func testAnAssistantTurnMayOpenWithATool() {
        let turns = ChatTranscript.build([use(1, "Read")])
        XCTAssertEqual(turns.map(\.role), [.assistant])
        XCTAssertEqual(turns[0].calls.map(\.name), ["Read"])
    }

    func testUnknownEventKindsAreShownAsSystemNotes() throws {
        let json = """
            {"id":"5","sequence":3,"kind":{"state":"unknown","label":"Brand New"},"role":null,
             "created_at":"2026-10-22T10:00:00Z","text":"x","truncated":false,"tool_name":null}
            """
        let history = try JSONDecoder().decode(HistoryEvent.self, from: Data(json.utf8))
        XCTAssertEqual(ChatEvent(history).role, .system)
    }

    func testWireEventsMapToRoles() throws {
        func history(_ kind: String, role: String?, tool: String? = nil) throws -> ChatEvent {
            let roleJSON = role.map { #"{"state":"known","value":"\#($0)"}"# } ?? "null"
            let toolJSON = tool.map { "\"\($0)\"" } ?? "null"
            let json = """
                {"id":"7","sequence":1,"kind":{"state":"known","value":"\(kind)"},"role":\(roleJSON),
                 "created_at":"2026-10-22T10:00:00Z","text":"x","truncated":false,"tool_name":\(toolJSON)}
                """
            return ChatEvent(try JSONDecoder().decode(HistoryEvent.self, from: Data(json.utf8)))
        }
        XCTAssertEqual(try history("Message", role: "User").role, .user)
        XCTAssertEqual(try history("Message", role: "Assistant").role, .assistant)
        let call = try history("ToolUse", role: "Assistant", tool: "Bash")
        XCTAssertEqual(call.role, .tool)
        XCTAssertEqual(call.phase, .use)
        XCTAssertEqual(call.toolName, "Bash")
        XCTAssertEqual(try history("ToolResult", role: nil).phase, .result)
        XCTAssertEqual(try history("Thinking", role: "Assistant").role, .thinking)
    }

    func testManagerConversationItemsMapOperatorToUserAndManagerToAssistant() throws {
        func item(_ speaker: String) throws -> ChatEvent {
            let json = """
                {"id":"3","sequence":2,"speaker":"\(speaker)","text":"x","truncated":false,
                 "created_at":"2026-10-22T10:00:00Z","operator_message_id":"m1"}
                """
            return ChatEvent(try JSONDecoder().decode(ConversationItem.self, from: Data(json.utf8)))
        }
        XCTAssertEqual(try item("operator").role, .user)
        XCTAssertEqual(try item("operator").operatorMessageId, "m1")
        XCTAssertEqual(try item("manager").role, .assistant)
        XCTAssertEqual(try item("system").role, .system)
    }
}

// MARK: Tool calls: folded by default

final class ToolFoldingTests: XCTestCase {
    func testAResultClosesTheCallBeforeIt() {
        let turns = ChatTranscript.build([use(1, "Bash", "ls"), result(2, "Bash", "a b")])
        let calls = turns[0].calls
        XCTAssertEqual(calls.count, 1)
        XCTAssertEqual(calls[0].use?.text, "ls")
        XCTAssertEqual(calls[0].result?.text, "a b")
        XCTAssertFalse(calls[0].running)
    }

    func testACallWithoutAResultIsRunning() {
        let call = ChatTranscript.build([use(1, "Bash")])[0].calls[0]
        XCTAssertTrue(call.running)
        XCTAssertEqual(toolSummary([call]), "Using Bash")
    }

    func testParallelCallsPairOldestFirst() {
        let calls = ChatTranscript.build([use(1, "Read"), use(2, "Grep"), result(3), result(4)])[0].calls
        XCTAssertEqual(calls.map(\.name), ["Read", "Grep"])
        XCTAssertEqual(calls.map { $0.result?.id }, ["3", "4"])
    }

    func testAResultWithNoCallStandsAlone() {
        let calls = ChatTranscript.build([result(1, "Bash", "late")])[0].calls
        XCTAssertEqual(calls.count, 1)
        XCTAssertNil(calls[0].use)
        XCTAssertEqual(calls[0].name, "Bash")
    }

    func testARunOfCallsIsOneFoldedPart() {
        let turns = ChatTranscript.build([
            use(1, "Bash"), result(2), use(3, "Read"), result(4), use(5, "Bash"), result(6), event(7, .assistant, "done"),
        ])
        XCTAssertEqual(turns[0].parts.count, 2, "one folded run, then the reply")
        XCTAssertEqual(turns[0].calls.count, 3)
        XCTAssertEqual(toolSummary(turns[0].calls), "Used Bash, Read · 3 calls")
    }

    func testTextBetweenCallsSplitsTheRun() {
        let turns = ChatTranscript.build([use(1, "A"), result(2), event(3, .assistant, "mid"), use(4, "B"), result(5)])
        XCTAssertEqual(turns[0].parts.count, 3)
    }

    func testTheSummaryNamesAtMostThreeToolsAndCountsTheRest() {
        let calls = ["A", "B", "C", "D", "E"].map { ToolCall(use: use(Int($0.unicodeScalars.first!.value), $0), result: result(1)) }
        XCTAssertEqual(toolSummary(calls), "Used A, B, C +2 · 5 calls")
    }

    func testEverythingStartsFoldedAndOpensByItsId() {
        let turns = ChatTranscript.build([event(1, .thinking, "hm"), use(2, "Bash"), result(3)])
        var disclosure = Disclosure()
        for part in turns[0].parts { XCTAssertFalse(disclosure.isOpen(part.id), "\(part.id) starts folded") }
        let tools = turns[0].parts[1]
        disclosure.toggle(tools.id)
        XCTAssertTrue(disclosure.isOpen(tools.id))
        XCTAssertFalse(disclosure.isOpen(turns[0].parts[0].id), "opening one leaves the others folded")
        disclosure.toggle(tools.id)
        XCTAssertFalse(disclosure.isOpen(tools.id))
    }

    func testAPartKeepsItsIdWhenMoreOutputFollows() {
        let before = ChatTranscript.build([use(1, "Bash")])[0].parts[0].id
        let after = ChatTranscript.build([use(1, "Bash"), result(2), use(3, "Read")])[0].parts[0].id
        XCTAssertEqual(before, after, "an opened row stays open as the run grows")
    }

    func testAFoldedBodyShowsTheFirstLinesAndSaysItWasCut() {
        let long = (1...40).map { "line \($0)" }.joined(separator: "\n")
        let preview = clippedPreview(long, maxLines: 6)
        XCTAssertEqual(preview.text.split(separator: "\n").count, 6)
        XCTAssertTrue(preview.cut)
        XCTAssertFalse(clippedPreview("short").cut)
        XCTAssertEqual(clippedPreview("short").text, "short")
        XCTAssertEqual(clippedPreview("").text, "")
    }

    func testAVeryLongLineIsClipped() {
        let preview = clippedPreview(String(repeating: "x", count: 1_000_000), maxCharacters: 100)
        XCTAssertEqual(preview.text.count, 100)
        XCTAssertTrue(preview.cut)
    }
}

// MARK: Streaming append

final class StreamingAppendTests: XCTestCase {
    func testNewOutputGrowsTheLastTurnAndKeepsEveryId() {
        var events = [event(1, .user, "go"), event(2, .assistant, "working")]
        let first = ChatTranscript.build(events)
        events += [use(3, "Bash"), result(4), event(5, .assistant, "done")]
        let next = ChatTranscript.build(events)
        XCTAssertEqual(next.map(\.id), first.map(\.id), "no new turn: the assistant kept going")
        XCTAssertEqual(next[1].parts.count, 3)
        XCTAssertEqual(next[1].eventCount, 4)
        XCTAssertEqual(next[0], first[0], "the earlier turn is untouched")
    }

    func testANewUserMessageOpensANewTurnWithoutRenamingOthers() {
        let before = ChatTranscript.build([event(1, .user), event(2, .assistant)])
        let after = ChatTranscript.build([event(1, .user), event(2, .assistant), event(3, .user)])
        XCTAssertEqual(Array(after.prefix(2)).map(\.id), before.map(\.id))
        XCTAssertEqual(after.last?.id, "3")
    }

    func testTheNewestEventMovesOnlyWhenNewerOutputArrives() {
        let base = ChatTranscript.build([event(5, .user), event(6, .assistant)])
        let older = ChatTranscript.build([event(3, .user), event(4, .assistant), event(5, .user), event(6, .assistant)])
        XCTAssertEqual(ChatTranscript.newest(base).id, "6")
        XCTAssertEqual(ChatTranscript.newest(older).id, "6", "older history above does not move the tail")
        XCTAssertEqual(ChatTranscript.newest(older).count, 4)
        let more = ChatTranscript.build([event(5, .user), event(6, .assistant), use(7, "Bash")])
        XCTAssertEqual(ChatTranscript.newest(more).id, "7")
        let closed = ChatTranscript.build([event(5, .user), event(6, .assistant), use(7, "Bash"), result(8)])
        XCTAssertEqual(ChatTranscript.newest(closed).id, "8", "a result is newer output")
    }

    func testAnOlderPageMergesIntoTheFirstTurnAtTheSeam() {
        let newer = ChatTranscript.build([event(5, .assistant, "b"), event(6, .assistant, "c")])
        let both = ChatTranscript.build([event(4, .assistant, "a"), event(5, .assistant, "b"), event(6, .assistant, "c")])
        XCTAssertEqual(newer.count, 1)
        XCTAssertEqual(both.count, 1)
        XCTAssertEqual(both[0].parts.count, 3)
    }
}

// MARK: Empty and very long sessions

final class LongSessionTests: XCTestCase {
    func testAVeryLongSessionGroupsInOnePass() {
        var events: [ChatEvent] = []
        var id = 0
        for _ in 0..<5_000 {
            id += 1; events.append(event(id, .user, "q"))
            id += 1; events.append(event(id, .thinking, "hm"))
            id += 1; events.append(use(id, "Bash"))
            id += 1; events.append(result(id))
            id += 1; events.append(event(id, .assistant, "a"))
        }
        let started = Date()
        let turns = ChatTranscript.build(Array(events.reversed()))
        XCTAssertLessThan(Date().timeIntervalSince(started), 5)
        XCTAssertEqual(turns.count, 10_000)
        XCTAssertEqual(turns.reduce(0) { $0 + $1.eventCount }, 25_000)
        XCTAssertEqual(turns.first?.id, "1")
        XCTAssertEqual(turns.last?.role, .assistant)
    }

    func testOneVeryLongMessageIsOneTurn() {
        let big = String(repeating: "word ", count: 200_000)
        let turns = ChatTranscript.build([event(1, .assistant, big)])
        XCTAssertEqual(turns.count, 1)
        guard case .text(let shown) = turns[0].parts[0] else { return XCTFail() }
        XCTAssertEqual(shown.text.count, big.count)
    }

    func testAnEmptyTextStillMakesATurn() {
        let turns = ChatTranscript.build([event(1, .assistant, "")])
        XCTAssertEqual(turns.count, 1)
    }
}

// MARK: Following the tail

final class TailFollowTests: XCTestCase {
    func testTheFirstContentScrollsToTheLatest() {
        var follow = TailFollow()
        XCTAssertFalse(follow.update(newestID: nil, eventCount: 0), "nothing to show yet")
        XCTAssertTrue(follow.update(newestID: "9", eventCount: 3))
    }

    func testNewOutputFollowsWhileAtTheBottom() {
        var follow = TailFollow()
        _ = follow.update(newestID: "9", eventCount: 3)
        follow.scrolled(distanceFromBottom: 10)
        XCTAssertTrue(follow.update(newestID: "10", eventCount: 4))
        XCTAssertEqual(follow.unseen, 0)
    }

    func testNewOutputWhileReadingAboveIsCountedNotScrolledTo() {
        var follow = TailFollow()
        _ = follow.update(newestID: "9", eventCount: 3)
        follow.scrolled(distanceFromBottom: 900)
        XCTAssertFalse(follow.atBottom)
        XCTAssertFalse(follow.update(newestID: "11", eventCount: 5))
        XCTAssertEqual(follow.unseen, 2)
        XCTAssertFalse(follow.update(newestID: "12", eventCount: 6))
        XCTAssertEqual(follow.unseen, 3)
        follow.scrolled(distanceFromBottom: 0)
        XCTAssertEqual(follow.unseen, 0, "reaching the bottom clears the count")
    }

    func testLoadingOlderHistoryNeverMovesTheReader() {
        var follow = TailFollow()
        _ = follow.update(newestID: "9", eventCount: 3)
        follow.scrolled(distanceFromBottom: 900)
        XCTAssertFalse(follow.update(newestID: "9", eventCount: 28))
        XCTAssertEqual(follow.unseen, 0)
        follow.scrolled(distanceFromBottom: 0)
        XCTAssertFalse(follow.update(newestID: "9", eventCount: 50))
    }

    func testTheJumpButtonAndSendingReturnToTheLatest() {
        var follow = TailFollow()
        _ = follow.update(newestID: "9", eventCount: 3)
        follow.scrolled(distanceFromBottom: 900)
        _ = follow.update(newestID: "10", eventCount: 4)
        follow.jumpedToLatest()
        XCTAssertTrue(follow.atBottom)
        XCTAssertEqual(follow.unseen, 0)
    }

    func testSlackKeepsAlmostAtTheBottomAtTheBottom() {
        var follow = TailFollow()
        follow.scrolled(distanceFromBottom: TailFollow.slack)
        XCTAssertTrue(follow.atBottom)
        follow.scrolled(distanceFromBottom: TailFollow.slack + 1)
        XCTAssertFalse(follow.atBottom)
    }
}

// MARK: The outbox in the conversation

final class PendingOutgoingTests: XCTestCase {
    private func queued(_ id: String, at seconds: Double, _ change: (inout OutboxMessage) -> Void = { _ in }) -> OutboxMessage {
        var m = OutboxMessage(
            id: id, host: "h", project: "p", role: "project_manager", seat: "s", text: id,
            createdAt: Date(timeIntervalSince1970: seconds))
        change(&m)
        return m
    }

    private func item(operatorMessageId: String?) throws -> ConversationItem {
        let id = operatorMessageId.map { "\"\($0)\"" } ?? "null"
        let json = """
            {"id":"3","sequence":2,"speaker":"operator","text":"x","truncated":false,
             "created_at":"2026-10-22T10:00:00Z","operator_message_id":\(id)}
            """
        return try JSONDecoder().decode(ConversationItem.self, from: Data(json.utf8))
    }

    func testQueuedSendingAndFailedStayAsBubblesOldestFirst() {
        let list = pendingOutgoing(
            [queued("b", at: 20), queued("a", at: 10) { $0.state = .sending }, queued("c", at: 30) { $0.state = .failed }],
            conversation: [])
        XCTAssertEqual(list.map(\.id), ["a", "b", "c"])
    }

    func testADeliveredMessageTheConversationAlreadyShowsLeavesTheBubbles() throws {
        let delivered = queued("a", at: 10) { $0.state = .delivered; $0.hubMessageId = "hub-1" }
        XCTAssertEqual(pendingOutgoing([delivered], conversation: []).map(\.id), ["a"], "not in the conversation yet")
        XCTAssertEqual(
            pendingOutgoing([delivered], conversation: [try item(operatorMessageId: "hub-1")]).map(\.id), [])
        XCTAssertEqual(
            pendingOutgoing([delivered], conversation: [try item(operatorMessageId: "other")]).map(\.id), ["a"])
    }

    func testAQueuedMessageStaysEvenIfAnotherIsInTheConversation() throws {
        let list = pendingOutgoing([queued("a", at: 10)], conversation: [try item(operatorMessageId: "hub-1")])
        XCTAssertEqual(list.map(\.id), ["a"])
    }
}

// MARK: Dark and light

final class AppearanceTests: XCTestCase {
    func testDarkIsTheDefaultAndAnUnknownValueFallsBack() {
        XCTAssertEqual(Appearance.default, .dark)
        XCTAssertEqual(Appearance.named(nil), .dark)
        XCTAssertEqual(Appearance.named("sepia"), .dark)
        XCTAssertEqual(Appearance.named("light"), .light)
    }

    func testMatchPhoneFollowsTheSystem() {
        XCTAssertTrue(Appearance.system.isLight(systemIsDark: false))
        XCTAssertFalse(Appearance.system.isLight(systemIsDark: true))
        XCTAssertTrue(Appearance.light.isLight(systemIsDark: true))
        XCTAssertFalse(Appearance.dark.isLight(systemIsDark: false))
    }

    func testEachThemeHasALightVariantThatKeepsItsIdentity() {
        for theme in RemoteTheme.all {
            let light = theme.resolved(light: true)
            XCTAssertTrue(light.isLight)
            XCTAssertEqual(light.key, theme.key)
            XCTAssertEqual(light.displayName, theme.displayName)
            XCTAssertEqual(light.loader, theme.loader)
            XCTAssertFalse(theme.isLight)
            XCTAssertEqual(theme.resolved(light: false), theme)
            XCTAssertEqual(light.lightVariant, light, "deriving twice changes nothing")
        }
    }

    func testTheLightCanvasIsLightAndTheTextReadsOnIt() {
        for theme in RemoteTheme.all {
            let dark = theme
            let light = theme.lightVariant
            XCTAssertGreaterThan(light.canvas.luminance, dark.canvas.luminance)
            XCTAssertGreaterThan(light.canvas.luminance, 0.5)
            // Body text, timestamps, section labels and the user/assistant/tool names on both surfaces.
            for surface in [light.canvas, light.panel, light.elevated] {
                XCTAssertGreaterThanOrEqual(light.primaryText.contrast(with: surface), 4.5, theme.key)
                XCTAssertGreaterThanOrEqual(light.mutedText.contrast(with: surface), 4.5, theme.key)
                XCTAssertGreaterThanOrEqual(light.assistantRole.contrast(with: surface), 3.0, theme.key)
                XCTAssertGreaterThanOrEqual(light.mdLink.contrast(with: surface), 3.0, theme.key)
                XCTAssertGreaterThanOrEqual(light.primary.contrast(with: surface), 3.0, "\(theme.key) primary")
            }
            XCTAssertGreaterThanOrEqual(light.error.contrast(with: light.canvas), 3.0)
            XCTAssertGreaterThanOrEqual(dark.primaryText.contrast(with: dark.canvas), 7.0, "dark stays readable")
        }
    }

    func testTheAppearanceChoiceIsWhatSettingsWrites() {
        // The key and the raw values are the stored contract; do not change them silently.
        XCTAssertEqual(Appearance.key, "rsi.remote.appearance")
        XCTAssertEqual(Appearance.allCases.map(\.rawValue).sorted(), ["dark", "light", "system"])
    }
}

// MARK: Rows

final class ChatRowTests: XCTestCase {
    func testALongAssistantTurnIsManySmallRows() {
        let turns = ChatTranscript.build([
            event(1, .user, "q"), event(2, .assistant, "a"), use(3, "Bash"), result(4), event(5, .assistant, "b"),
        ])
        let rows = ChatTranscript.rows(turns)
        // user: bubble + time; assistant: header + text + folded tools + text.
        XCTAssertEqual(rows.count, 2 + 4)
        XCTAssertEqual(rows.filter(\.startsTurn).map(\.turnID), ["1", "2"])
        XCTAssertEqual(Set(rows.map(\.id)).count, rows.count, "row ids are unique")
    }

    func testRowIdsStayPutWhileNewerOutputAppends() {
        let before = ChatTranscript.rows(ChatTranscript.build([event(1, .user), event(2, .assistant)]))
        let after = ChatTranscript.rows(
            ChatTranscript.build([event(1, .user), event(2, .assistant), use(3, "Bash"), event(4, .user)]))
        XCTAssertEqual(Array(after.prefix(before.count)).map(\.id), before.map(\.id))
    }

    func testNoRowsForNoTurns() {
        XCTAssertEqual(ChatTranscript.rows([]), [])
    }

    func testSystemNotesGetATimeStampToo() {
        let rows = ChatTranscript.rows(ChatTranscript.build([event(1, .system, "rotated")]))
        XCTAssertEqual(rows.count, 2)
        guard case .stamp = rows[1].kind else { return XCTFail() }
    }
}

final class TailFollowGrowthTests: XCTestCase {
    func testContentGrowingUnderAReaderAtTheBottomDoesNotMakeThemLeaveIt() {
        var follow = TailFollow()
        follow.scrolled(distanceFromBottom: 0, contentHeight: 1000)
        // Three new rows are laid out: the content is taller, the reader has not moved.
        follow.scrolled(distanceFromBottom: 420, contentHeight: 1420)
        XCTAssertTrue(follow.atBottom)
        XCTAssertTrue(follow.update(newestID: "a", eventCount: 1), "so the new output is scrolled to")
        XCTAssertTrue(follow.update(newestID: "b", eventCount: 4))
        // Scrolled to the end: same height, small distance.
        follow.scrolled(distanceFromBottom: 0, contentHeight: 1420)
        XCTAssertTrue(follow.atBottom)
    }

    func testAReaderWhoScrollsUpIsAwayEvenWhenTheHeightIsSteady() {
        var follow = TailFollow()
        follow.scrolled(distanceFromBottom: 0, contentHeight: 1000)
        follow.scrolled(distanceFromBottom: 600, contentHeight: 1000)
        XCTAssertFalse(follow.atBottom)
    }

    func testContentGrowingUnderAReaderAboveStaysAbove() {
        var follow = TailFollow()
        follow.scrolled(distanceFromBottom: 600, contentHeight: 1000)
        follow.scrolled(distanceFromBottom: 600, contentHeight: 1000)
        follow.scrolled(distanceFromBottom: 1000, contentHeight: 1400)
        XCTAssertFalse(follow.atBottom)
    }
}
