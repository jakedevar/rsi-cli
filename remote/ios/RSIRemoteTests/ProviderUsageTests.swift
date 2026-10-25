import Foundation
import XCTest

@testable import RSIRemote

// Provider usage (#504), `GET /api/v2/providers` (hub #3796): the contract's
// own fixtures and the app's relative-time fixtures decode through the
// tolerant wire type; the rows the screen shows (windows, reset countdowns,
// overage, spend as an API-equivalent estimate, reason codes in words, "no
// data"); the store's refresh, failure and cancellation rules and its read of
// the hub route.

final class ProviderUsageDecodingTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_800_000_000)

    private func decode(_ json: String) throws -> ProviderUsage {
        try JSONDecoder().decode(ProviderUsage.self, from: Data(json.utf8))
    }

    func testTheFullFixtureDecodesEveryProviderWithItsWindows() throws {
        let usage = try ProviderUsageFixture.decoded(.full, now: now)
        XCTAssertEqual(usage.providers.map(\.provider), ["Claude", "Codex", "OpenRouter", "Bedrock"])
        XCTAssertEqual(usage.providers[0].windows.map(\.key), ["five_hour", "seven_day"])
        XCTAssertEqual(usage.providers[0].windows.map(\.utilization), [0.42, 0.81])
        XCTAssertEqual(usage.providers[0].bindingWindow, "five_hour")
        XCTAssertEqual(usage.providers[0].overageStatus, "rejected")
        XCTAssertEqual(usage.providers[0].models.map(\.model), ["claude-opus-5", "claude-sonnet-5-5"])
        XCTAssertEqual(usage.providers[2].spend, UsageSpend(cost1hUSD: 0.4, cost24hUSD: 6.1))
        XCTAssertEqual(usage.providers[2].windowsReason, "no_observation")
        XCTAssertEqual(usage.providers[3].spendReason, "warming")
        XCTAssertFalse(usage.providersTruncated)
        XCTAssertFalse(usage.windowsIncomplete)
        XCTAssertEqual(usage.unreadableProviders, 0)
    }

    func testTheResetTimeIsReadFromTheStringOrTheEpoch() throws {
        let usage = try ProviderUsageFixture.decoded(.full, now: now)
        let claude5h = usage.providers[0].windows[0].resetsAt
        XCTAssertEqual(try XCTUnwrap(claude5h).timeIntervalSince(now), 2 * 3600 + 14 * 60, accuracy: 1)
        let codex = usage.providers[1].windows
        XCTAssertEqual(codex.map(\.key), ["primary", "secondary"])
        XCTAssertEqual(
            try XCTUnwrap(codex[0].resetsAt).timeIntervalSince(now), 47 * 60, accuracy: 1,
            "only resets_at_epoch given")
    }

    func testANullResetAndNullBlocksReadAsNotKnown() throws {
        let usage = try decode(
            #"{"providers":[{"provider":"X","status":null,"windows":[{"key":"a","utilization":0.1,"#
                + #""resets_at":null,"resets_at_epoch":null}],"windows_reason":null,"spend":null,"#
                + #""spend_reason":null,"models":null,"overage_status":null}]}"#)
        let entry = usage.providers[0]
        XCTAssertNil(entry.windows[0].resetsAt)
        XCTAssertNil(entry.spend)
        XCTAssertNil(entry.spendReason)
        XCTAssertEqual(entry.models, [])
        XCTAssertTrue(entry.hasData, "a window is data")
    }

    func testTheContractFixturesDecodeToTheirDocumentedValues() throws {
        let full = try V2Fixture.named("providers_200").decode(ProviderUsage.self)
        XCTAssertEqual(full.providers.map(\.provider), ["Claude", "Codex"])
        let claude = full.providers[0]
        XCTAssertEqual(claude.status, "allowed")
        XCTAssertEqual(claude.bindingWindow, "five_hour")
        XCTAssertEqual(claude.overageStatus, "rejected")
        XCTAssertFalse(claude.isUsingOverage)
        XCTAssertEqual(claude.windows.map(\.key), ["five_hour", "seven_day"])
        XCTAssertEqual(claude.windows.map(\.utilization), [0.42, 0.1])
        XCTAssertEqual(claude.windows[0].resetsAt, parseWireTime("2026-10-22T15:00:00.000000000Z"))
        XCTAssertEqual(claude.windows[0].resetsAt, Date(timeIntervalSince1970: 1_792_681_200))
        XCTAssertNil(claude.windows[1].resetsAt)
        XCTAssertEqual(claude.spend, UsageSpend(cost1hUSD: 1.25, cost24hUSD: 9.5))
        XCTAssertEqual(claude.models.map(\.model), ["claude-opus", "claude-haiku"])
        XCTAssertEqual(full.providers[1].staleReason, "probe_timeout")
        XCTAssertEqual(full.providers[1].status, "allowed_warning")
        XCTAssertEqual(full.providers[1].windows[0].utilization, 0.97)
        XCTAssertFalse(full.providersTruncated)
        XCTAssertFalse(full.windowsIncomplete)
        XCTAssertEqual(full.observedAt, "2026-10-22T12:00:00.000000000Z")
    }

    func testTheContractEmptyFixtureHasProvidersWithReasonsAndNoData() throws {
        let empty = try V2Fixture.named("providers_empty_200").decode(ProviderUsage.self)
        XCTAssertEqual(empty.providers.map(\.provider), ["Claude", "Codex"])
        XCTAssertEqual(empty.providers.map(\.hasData), [false, false])
        XCTAssertEqual(empty.providers.map(\.windowsReason), ["no_observation", "no_observation"])
        XCTAssertEqual(empty.providers.map(\.spendReason), ["warming", "warming"])
        XCTAssertNil(empty.providers[0].status)
        XCTAssertEqual(empty.summary, "2 providers · 0 with data")
        let rows = empty.rows(now: parseWireTime("2026-10-22T12:00:30Z")!)
        XCTAssertEqual(rows[0].noDataText, "No usage data from Claude yet.")
        XCTAssertEqual(rows[0].reasonNotes, ["No plan-window reading yet.", "Spend is still warming up."])
        XCTAssertNil(rows[0].spendLabel)
    }

    func testTheContractRowsLabelSpendAndShowStaleness() throws {
        let full = try V2Fixture.named("providers_200").decode(ProviderUsage.self)
        let rows = full.rows(now: parseWireTime("2026-10-22T12:00:30Z")!)
        XCTAssertEqual(rows[0].spend, "$1.25 last hour · $9.50 last 24 h")
        XCTAssertEqual(rows[0].spendLabel, "API-equivalent estimate")
        XCTAssertEqual(rows[0].reasonNotes, [])
        XCTAssertEqual(rows[0].windows.map(\.resetText), ["Resets in 2 h 59 min", "Reset time not reported"])
        XCTAssertEqual(rows[1].staleNote, "Last known values; the latest refresh failed (probe timeout).")
        XCTAssertEqual(rows[1].windows[0].tone, .error)
        XCTAssertEqual(rows[1].status, "Near limit")
    }

    func testTheTopLevelAndProviderFlagsAreRead() throws {
        let usage = try decode(
            #"{"providers":[{"provider":"Claude","windows_truncated":true,"models_truncated":true,"#
                + #""windows":[{"key":"w","utilization":0.2}],"spend_reason":"stale"}],"#
                + #""providers_truncated":true,"windows_incomplete":true,"observed_at":"2026-10-22T12:00:00Z"}"#)
        XCTAssertTrue(usage.windowsIncomplete)
        XCTAssertEqual(usage.incompleteNote, "The hub lists only some providers. Some plan-window readings may be missing.")
        let row = usage.rows(now: now)[0]
        XCTAssertEqual(
            row.reasonNotes,
            ["Only some plan windows are listed.", "Spend figures are out of date.", "Only the largest models are listed."])
    }

    func testTheOldCreditsBlockAndAliasesAreIgnored() throws {
        let usage = try decode(
            #"{"providers":[{"provider":"OpenRouter","credits":{"remaining_usd":12.34,"limit_usd":50},"#
                + #""windows":[{"window_key":"old","utilization":0.5},{"key":"new","utilization":0.5,"resets_at":1800000600}]}]}"#)
        let entry = usage.providers[0]
        XCTAssertEqual(entry.windows.map(\.key), ["new"], "window_key is not part of the contract")
        XCTAssertNil(entry.windows[0].resetsAt, "a number under resets_at is not a time")
        let shown = String(describing: ProviderUsageRow(entry, now: now))
        XCTAssertFalse(shown.contains("12.34"), "no credits are shown")
    }

    func testUnknownAndMissingFieldsAreTolerated() throws {
        XCTAssertEqual(try decode("{}"), ProviderUsage())
        let usage = try decode(
            #"{"providers":[{"provider":"Claude","extra":{"a":1},"status":42,"windows":"nope","credits":7,"spend":[]}],"future":1}"#)
        XCTAssertEqual(usage.providers.map(\.provider), ["Claude"])
        let claude = usage.providers[0]
        XCTAssertNil(claude.status, "a wrongly typed field reads as absent")
        XCTAssertEqual(claude.windows, [])
        XCTAssertNil(claude.spend)
        XCTAssertFalse(claude.hasData)
    }

    func testABrokenProviderOrWindowIsDroppedNotFatal() throws {
        let usage = try decode(
            #"{"providers":[{"provider":5},{"windows":[]},{"provider":"  "},"#
                + #"{"provider":"Codex","windows":[{"utilization":0.5},{"key":"w","utilization":"bad"},"#
                + #"{"key":"ok","utilization":-1},{"key":"good","utilization":0.25}]}]}"#)
        XCTAssertEqual(usage.providers.map(\.provider), ["Codex"])
        XCTAssertEqual(usage.unreadableProviders, 3)
        XCTAssertEqual(usage.incompleteNote, "3 providers could not be read.")
        let windows = usage.providers[0].windows
        XCTAssertEqual(windows.map(\.key), ["w", "ok", "good"], "a window without a key is dropped")
        XCTAssertEqual(windows.map(\.utilization), [nil, nil, 0.25], "bad or negative readings read as no reading")
    }

    func testAProviderWithOnlyANameHasNoData() throws {
        let usage = try decode(#"{"providers":[{"provider":"Bedrock"}]}"#)
        XCTAssertFalse(usage.providers[0].hasData)
        XCTAssertEqual(usage.summary, "1 provider · 0 with data")
    }

    func testTheEntryDecodesOnlyTheFieldsTheHubIsAskedFor() throws {
        // The assumed projection carries no account, e-mail, key or token field: a hub that
        // sent one would be ignored, not shown. Nothing here decodes such a name.
        let json = #"{"providers":[{"provider":"Claude","account_email":"x@y.z","api_key":"sk-1","token":"t","#
            + #""windows":[{"key":"five_hour","utilization":0.5,"org_id":"o"}]}]}"#
        let entry = try decode(json).providers[0]
        let shown = String(describing: ProviderUsageRow(entry, now: now))
        XCTAssertFalse(shown.contains("sk-1") || shown.contains("x@y.z"))
        XCTAssertEqual(entry.windows.map(\.key), ["five_hour"])
    }
}

final class ProviderUsageRowTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_800_000_000)

    private func rows(_ variant: ProviderUsageFixture.Variant = .full) throws -> [ProviderUsageRow] {
        try ProviderUsageFixture.decoded(variant, now: now).rows(now: now)
    }

    func testAWindowShowsItsUtilisationBarToneAndResetCountdown() throws {
        let claude = try rows()[0]
        XCTAssertEqual(claude.name, "Claude")
        XCTAssertEqual(claude.status, "Within limits")
        XCTAssertEqual(claude.statusTone, .ok)
        XCTAssertEqual(claude.windows.map(\.label), ["5-hour", "7-day"])
        XCTAssertEqual(claude.windows.map(\.percentText), ["42%", "81%"])
        XCTAssertEqual(claude.windows.map(\.fraction), [0.42, 0.81])
        XCTAssertEqual(claude.windows.map(\.tone), [.ok, .warn])
        XCTAssertEqual(claude.windows.map(\.resetText), ["Resets in 2 h 14 min", "Resets in 3 d 4 h"])
        XCTAssertEqual(claude.windows.map(\.isBinding), [true, false])
    }

    func testANearlyFullWindowIsAnErrorAndAStaleProviderSaysWhy() throws {
        let codex = try rows()[1]
        XCTAssertEqual(codex.status, "Near limit")
        XCTAssertEqual(codex.statusTone, .warn)
        XCTAssertEqual(codex.windows.map(\.label), ["Primary", "Secondary"])
        XCTAssertEqual(codex.windows[0].percentText, "93%")
        XCTAssertEqual(codex.windows[0].tone, .error)
        XCTAssertEqual(codex.windows[0].resetText, "Resets in 47 min")
        XCTAssertEqual(codex.staleNote, "Last known values; the latest refresh failed (probe timeout).")
    }

    func testOverageAndSpendRead() throws {
        let all = try rows()
        XCTAssertEqual(all[0].overage, "Overage not available")
        XCTAssertEqual(all[0].overageTone, .idle)
        XCTAssertEqual(all[0].spend, "$3.20 last hour · $41.75 last 24 h")
        XCTAssertEqual(
            all[0].modelSpend.map(\.text),
            ["$2.60 last hour · $33.10 last 24 h", "$0.60 last hour · $8.65 last 24 h"])
        XCTAssertEqual(all[1].spend, "$0.00 last hour · $12.40 last 24 h")
        XCTAssertEqual(all[0].spendLabel, "API-equivalent estimate")
        XCTAssertEqual(all[2].windows, [], "a provider with spend only has no plan windows")
        XCTAssertEqual(all[2].reasonNotes, ["No plan-window reading yet."])
        XCTAssertEqual(all[2].spend, "$0.40 last hour · $6.10 last 24 h")
        XCTAssertTrue(all[2].hasData)
        XCTAssertEqual(all[1].reasonNotes, ["Only the largest models are listed."])
    }

    func testAProviderWithNoDataSaysSo() throws {
        let bedrock = try rows()[3]
        XCTAssertFalse(bedrock.hasData)
        XCTAssertEqual(bedrock.noDataText, "No usage data from Bedrock yet.")
        XCTAssertEqual(bedrock.windows, [])
        XCTAssertNil(bedrock.spend)
        XCTAssertNil(bedrock.spendLabel)
        XCTAssertEqual(bedrock.reasonNotes, ["No plan-window reading yet.", "Spend is still warming up."])
        XCTAssertNil(bedrock.status)
    }

    func testUsingOverageIsAWarningAndAFullWindowIsAnError() throws {
        let claude = try rows(.sparse)[0]
        XCTAssertEqual(claude.overage, "Using overage")
        XCTAssertEqual(claude.overageTone, .warn)
        XCTAssertEqual(claude.windows[0].percentText, "100%")
        XCTAssertEqual(claude.windows[0].tone, .error)
        XCTAssertEqual(claude.windows[0].resetText, "Resets in 25 min")
        XCTAssertEqual(claude.reasonNotes, ["Spend is incomplete: the usage read hit its cap."])
    }

    func testEverySpendAndWindowReasonCodeReadsAsWords() {
        XCTAssertEqual(usageSpendReasonText("warming"), "Spend is still warming up.")
        XCTAssertEqual(usageSpendReasonText("unavailable"), "Spend is unavailable right now.")
        XCTAssertEqual(usageSpendReasonText("stale"), "Spend figures are out of date.")
        XCTAssertEqual(usageSpendReasonText("truncated"), "Spend is incomplete: the usage read hit its cap.")
        XCTAssertEqual(usageSpendReasonText("new_code"), "Spend: new code.")
        XCTAssertEqual(usageWindowsReasonText("no_observation"), "No plan-window reading yet.")
        XCTAssertEqual(usageWindowsReasonText("other_code"), "Plan windows: other code.")
    }

    func testTheNoDataFixtureShowsThreeReasonedProviders() throws {
        let rows = try rows(.nodata)
        XCTAssertEqual(rows.map(\.name), ["OpenRouter", "Bedrock", "Local"])
        XCTAssertEqual(rows.map(\.hasData), [false, false, false])
        XCTAssertEqual(
            rows.map { $0.reasonNotes.last },
            ["Spend is still warming up.", "Spend is unavailable right now.", "Spend figures are out of date."])
    }

    func testTheEmptyFixtureHasNoProviders() throws {
        let usage = try ProviderUsageFixture.decoded(.empty, now: now)
        XCTAssertEqual(usage.rows(now: now), [])
        XCTAssertEqual(usage.summary, "No providers")
        XCTAssertNil(usage.incompleteNote)
    }

    func testTheSummaryCountsProvidersWithData() throws {
        XCTAssertEqual(try ProviderUsageFixture.decoded(.full, now: now).summary, "4 providers · 3 with data")
        XCTAssertEqual(try ProviderUsageFixture.decoded(.sparse, now: now).summary, "1 provider · 1 with data")
    }

    func testAPassedResetMarksTheReadingOldAndTheCountdownMoves() throws {
        let usage = try ProviderUsageFixture.decoded(.full, now: now)
        let later = now.addingTimeInterval(3 * 3600)
        let window = usage.rows(now: later)[0].windows[0]
        XCTAssertTrue(window.isExpired)
        XCTAssertEqual(window.resetText, "Reset 46 min ago; waiting for a new reading")
        let soon = usage.rows(now: now.addingTimeInterval(60))[0].windows[0]
        XCTAssertFalse(soon.isExpired)
        XCTAssertEqual(soon.resetText, "Resets in 2 h 13 min")
    }

    func testProviderNumbersOlderThanTenMinutesShowTheirAge() throws {
        let entry = ProviderUsageEntry(
            provider: "Claude", observedAt: "2026-10-23T10:00:00Z", windows: [UsageWindow(key: "five_hour", utilization: 0.1, resetsAt: nil)])
        let seen = parseWireTime("2026-10-23T10:00:00Z")!
        XCTAssertNil(ProviderUsageRow(entry, now: seen.addingTimeInterval(120)).staleNote)
        XCTAssertEqual(ProviderUsageRow(entry, now: seen.addingTimeInterval(1800)).staleNote, "Last seen 30 min ago.")
    }

    func testAWindowWithoutAReadingOrResetSaysSo() {
        let row = UsageWindowRow(UsageWindow(key: "monthly", utilization: nil, resetsAt: nil), binding: false, now: now)
        XCTAssertEqual(row.label, "Monthly")
        XCTAssertNil(row.fraction)
        XCTAssertEqual(row.percentText, "No reading")
        XCTAssertEqual(row.resetText, "Reset time not reported")
        XCTAssertEqual(row.tone, .idle)
    }

    func testOverOneHundredPercentKeepsTheNumberAndFillsTheBar() {
        let row = UsageWindowRow(UsageWindow(key: "five_hour", utilization: 1.2, resetsAt: nil), binding: true, now: now)
        XCTAssertEqual(row.percentText, "120%")
        XCTAssertEqual(row.fraction, 1)
        XCTAssertEqual(row.tone, .error)
    }

    func testTheToneThresholds() {
        func tone(_ value: Double) -> UsageTone {
            UsageWindowRow(UsageWindow(key: "k", utilization: value, resetsAt: nil), binding: false, now: now).tone
        }
        XCTAssertEqual([0, 0.74, 0.75, 0.89, 0.9, 1].map(tone), [.ok, .ok, .warn, .warn, .error, .error])
    }

    func testTruncationIsNamed() throws {
        let usage = try JSONDecoder().decode(
            ProviderUsage.self, from: Data(#"{"providers":[{"provider":"A"}],"providers_truncated":true}"#.utf8))
        XCTAssertEqual(usage.incompleteNote, "The hub lists only some providers.")
    }
}

final class UsageWordsTests: XCTestCase {
    func testWindowLabels() {
        XCTAssertEqual(usageWindowLabel("five_hour"), "5-hour")
        XCTAssertEqual(usageWindowLabel("seven_day_opus"), "7-day Opus")
        XCTAssertEqual(usageWindowLabel("ninety_day_burst"), "Ninety day burst")
        XCTAssertEqual(usageWindowLabel("Weekly"), "Weekly")
    }

    func testCountdowns() {
        XCTAssertEqual(resetCountdown(30), "under a minute")
        XCTAssertEqual(resetCountdown(60), "1 min")
        XCTAssertEqual(resetCountdown(45 * 60 + 59), "45 min")
        XCTAssertEqual(resetCountdown(2 * 3600), "2 h")
        XCTAssertEqual(resetCountdown(2 * 3600 + 14 * 60), "2 h 14 min")
        XCTAssertEqual(resetCountdown(86_400), "1 d")
        XCTAssertEqual(resetCountdown(3 * 86_400 + 4 * 3600 + 59 * 60), "3 d 4 h")
        XCTAssertEqual(resetCountdown(-5), "under a minute")
    }

    func testStatusAndOverageWords() {
        XCTAssertEqual(usageStatus("allowed").0, "Within limits")
        XCTAssertEqual(usageStatus("allowed_warning").1, .warn)
        XCTAssertEqual(usageStatus("rejected").0, "Limit reached")
        XCTAssertEqual(usageStatus("rejected").1, .error)
        XCTAssertEqual(usageStatus("something_new").0, "Something new")
        XCTAssertNil(usageStatus(nil).0)
        XCTAssertEqual(usageOverage(status: "allowed", using: false).0, "Overage available")
        XCTAssertEqual(usageOverage(status: "rejected", using: true).0, "Using overage", "using wins over the status")
        XCTAssertEqual(usageOverage(status: "paused_by_admin", using: false).0, "Overage: paused by admin")
        XCTAssertNil(usageOverage(status: nil, using: false).0)
    }

    func testMoney() {
        XCTAssertEqual(usd(0), "$0.00")
        XCTAssertEqual(usd(0.004), "<$0.01")
        XCTAssertEqual(usd(12.346), "$12.35")
        XCTAssertEqual(usd(1234.6), "$1,235")
        XCTAssertEqual(usd(-1.2), "-$1.20")
    }

    func testSpendText() {
        XCTAssertNil(usageSpendText(UsageSpend()))
        XCTAssertEqual(usageSpendText(UsageSpend(cost24hUSD: 2)), "$2.00 last 24 h")
    }
}

@MainActor
final class ProviderUsageStoreTests: XCTestCase {
    private let start = Date(timeIntervalSince1970: 1_800_000_000)

    private actor Script {
        private var results: [Result<ProviderUsage, Error>]
        private(set) var calls = 0
        init(_ results: [Result<ProviderUsage, Error>]) { self.results = results }
        func next() throws -> ProviderUsage {
            calls += 1
            let result = results.count > 1 ? results.removeFirst() : results[0]
            return try result.get()
        }
    }

    private func usage(_ names: String...) -> ProviderUsage {
        ProviderUsage(providers: names.map { ProviderUsageEntry(provider: $0) })
    }

    private func makeStore(_ script: Script, clock: @escaping @MainActor () -> Date) -> ProviderUsageStore {
        ProviderUsageStore(load: { try await script.next() }, now: clock)
    }

    func testARefreshShowsTheProjectionAndMarksItFresh() async {
        let script = Script([.success(usage("Claude"))])
        let store = makeStore(script, clock: { self.start })
        XCTAssertNil(store.usage)
        await store.refresh()
        XCTAssertEqual(store.usage?.providers.map(\.provider), ["Claude"])
        XCTAssertNil(store.error)
        XCTAssertEqual(store.freshness.updatedAt, start)
        XCTAssertFalse(store.loading)
    }

    func testAFailedRefreshKeepsTheLastGoodDataAndNamesTheFailure() async {
        let script = Script([.success(usage("Claude")), .failure(RemoteError.busy)])
        var clock = start
        let store = makeStore(script, clock: { clock })
        await store.refresh()
        clock = start.addingTimeInterval(60)
        await store.refresh()
        XCTAssertEqual(store.usage?.providers.map(\.provider), ["Claude"])
        XCTAssertEqual(store.error, .busy)
        XCTAssertEqual(
            store.freshness.status(now: clock, every: ProviderUsageStore.every),
            "Hub busy · showing data from 1 min ago · retrying in 5 s")
    }

    func testAFailureWithNothingYetLeavesNoDataAndAnError() async {
        let script = Script([.failure(RemoteError.unreachable("offline"))])
        let store = makeStore(script, clock: { self.start })
        await store.refresh()
        XCTAssertNil(store.usage)
        XCTAssertEqual(store.error, .unreachable("offline"))
    }

    func testACancelledReadIsNotAFailure() async {
        let script = Script([.success(usage("Claude")), .failure(CancellationError())])
        let store = makeStore(script, clock: { self.start })
        await store.refresh()
        await store.refresh()
        XCTAssertNil(store.error)
        XCTAssertEqual(store.usage?.providers.map(\.provider), ["Claude"])
    }

    func testAnUnknownErrorBecomesAHubError() async {
        struct Odd: Error {}
        let store = makeStore(Script([.failure(Odd())]), clock: { self.start })
        await store.refresh()
        XCTAssertEqual(store.error?.headline, "Hub error")
    }

    func testOpeningReadsAtOnceThenReadsAgainEveryMinute() async throws {
        let script = Script([.success(usage("Claude"))])
        let store = makeStore(script, clock: { Date() })
        let task = Task { await store.keepFresh() }
        try await Task.sleep(for: .milliseconds(300))
        let first = await script.calls
        XCTAssertEqual(first, 1, "read on open")
        XCTAssertNotNil(store.usage)
        task.cancel()
        await task.value
    }

    func testAScreenThatReopensWithinSecondsReusesTheRead() async throws {
        let script = Script([.success(usage("Claude"))])
        let store = makeStore(script, clock: { self.start })
        await store.refresh()
        let task = Task { await store.keepFresh(reuseFor: 15) }
        try await Task.sleep(for: .milliseconds(200))
        let calls = await script.calls
        XCTAssertEqual(calls, 1, "a read from this very second is reused")
        task.cancel()
        await task.value
    }

    func testResetForgetsEverything() async {
        let store = makeStore(Script([.success(usage("Claude"))]), clock: { self.start })
        await store.refresh()
        store.reset()
        XCTAssertNil(store.usage)
        XCTAssertNil(store.freshness.updatedAt)
    }

    func testTheFixtureSourcesLoad() async {
        let full = ProviderUsageStore(source: .fixture(.full))
        await full.refresh()
        XCTAssertEqual(full.usage?.providers.count, 4)
        let empty = ProviderUsageStore(source: .fixture(.empty))
        await empty.refresh()
        XCTAssertEqual(empty.usage?.providers.count, 0)
        XCTAssertNil(empty.error)
        let failing = ProviderUsageStore(source: .fixture(.failing))
        await failing.refresh()
        XCTAssertNil(failing.usage)
        XCTAssertEqual(failing.error, .server(status: 500, code: "fixture_failing"))
    }

    func testTheHubSourceWithNoSavedHostIsNotConfigured() async {
        let hub = ProviderUsageStore(source: .hub)
        await hub.refresh()
        XCTAssertEqual(hub.error, .notConfigured)
        XCTAssertNil(hub.usage)
    }

    private func hubClient() -> RemoteClient {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubProtocol.self]
        return RemoteClient(host: V2Fixture.host, session: URLSession(configuration: configuration), retry: .instant)
    }

    private func signIn() {
        StubGateway.shared.script("GET /auth/bootstrap", .json(#"{"nonce":"n-1"}"#))
        StubGateway.shared.script("POST /auth/session", .json(#"{"csrf":"csrf-1"}"#))
    }

    func testTheHubSourceReadsTheProvidersRouteAndShowsTheContractBody() async throws {
        StubGateway.shared.reset()
        signIn()
        StubGateway.shared.script("GET /api/v2/providers", try V2Fixture.named("providers_200").reply)
        let client = hubClient()
        let store = ProviderUsageStore(source: .hub, client: { client }, now: { self.start })
        await store.refresh()
        XCTAssertNil(store.error)
        XCTAssertEqual(store.usage?.providers.map(\.provider), ["Claude", "Codex"])
        let seen = StubGateway.shared.requests.filter { $0.path == "/api/v2/providers" }
        XCTAssertEqual(seen.count, 1)
        XCTAssertEqual(seen[0].method, "GET")
        XCTAssertEqual(seen[0].query, nil, "the route takes no query")
        XCTAssertTrue(seen[0].body.isEmpty)
    }

    func testABusyHubKeepsTheLastProvidersAndNamesTheFailure() async throws {
        StubGateway.shared.reset()
        signIn()
        StubGateway.shared.script(
            "GET /api/v2/providers", try V2Fixture.named("providers_200").reply,
            try V2Fixture.named("providers_busy_503").reply)
        let client = hubClient()
        var clock = start
        let store = ProviderUsageStore(source: .hub, client: { client }, now: { clock })
        await store.refresh()
        clock = start.addingTimeInterval(60)
        await store.refresh()
        XCTAssertEqual(store.usage?.providers.count, 2, "the last good projection stays")
        XCTAssertEqual(store.error, .busy)
    }

    func testTheHubStoreFollowsTheSavedHostsClientAtEveryRead() async throws {
        StubGateway.shared.reset()
        signIn()
        StubGateway.shared.script("GET /api/v2/providers", try V2Fixture.named("providers_empty_200").reply)
        var current: RemoteClient?
        let store = ProviderUsageStore(source: .hub, client: { current }, now: { self.start })
        await store.refresh()
        XCTAssertEqual(store.error, .notConfigured)
        current = hubClient()
        await store.refresh()
        XCTAssertNil(store.error)
        XCTAssertEqual(store.usage?.providers.count, 2)
    }

    func testTheLaunchArgumentPicksAFixtureAndNothingElseDoes() {
        XCTAssertEqual(ProviderUsageSource.parse(nil), .hub)
        XCTAssertEqual(ProviderUsageSource.parse("nonsense"), .hub)
        XCTAssertEqual(ProviderUsageSource.parse(" Full "), .fixture(.full))
        XCTAssertEqual(ProviderUsageSource.parse("sparse"), .fixture(.sparse))
        XCTAssertTrue(ProviderUsageSource.parse("empty").isFixture)
        XCTAssertFalse(ProviderUsageSource.hub.isFixture, "the hub is read over the network, not offline")
        XCTAssertEqual(ProviderUsageSource.parse("nodata"), .fixture(.nodata))
        XCTAssertEqual(ProviderUsageSource.parse("credits"), .hub, "the retired credits fixture is no fixture")
        let defaults = UserDefaults(suiteName: "provider-usage-tests")!
        defaults.removePersistentDomain(forName: "provider-usage-tests")
        XCTAssertEqual(ProviderUsageSource.current(defaults), .hub)
        defaults.set("full", forKey: ProviderUsageSource.launchKey)
        XCTAssertEqual(ProviderUsageSource.current(defaults), .fixture(.full))
        defaults.removePersistentDomain(forName: "provider-usage-tests")
    }
}

@MainActor
final class ProviderUsageDashboardTests: XCTestCase {
    func testTheAppModelHandsTheSavedHostsClientToTheUsageStore() async throws {
        StubGateway.shared.reset()
        StubGateway.shared.script("GET /auth/bootstrap", .json(#"{"nonce":"n-1"}"#))
        StubGateway.shared.script("POST /auth/session", .json(#"{"csrf":"csrf-1"}"#))
        StubGateway.shared.script("GET /api/v2/providers", try V2Fixture.named("providers_200").reply)
        let defaults = UserDefaults(suiteName: "provider-usage-model-tests")!
        defaults.removePersistentDomain(forName: "provider-usage-model-tests")
        let model = AppModel(
            defaults: defaults,
            makeClient: { host in
                let configuration = URLSessionConfiguration.ephemeral
                configuration.protocolClasses = [StubProtocol.self]
                return RemoteClient(host: host, session: URLSession(configuration: configuration), retry: .instant)
            })
        XCTAssertEqual(model.providerUsageSource, .hub)
        await model.providerUsage.refresh()
        XCTAssertEqual(model.providerUsage.error, .notConfigured, "no host saved yet")
        model.setHost(V2Fixture.host)
        await model.providerUsage.refresh()
        XCTAssertNil(model.providerUsage.error)
        XCTAssertEqual(model.providerUsage.usage?.providers.map(\.provider), ["Claude", "Codex"])
        model.setHost("other.example.ts.net")
        XCTAssertNil(model.providerUsage.usage, "a new host forgets the old host's numbers")
        defaults.removePersistentDomain(forName: "provider-usage-model-tests")
    }
}
