import Foundation
import Observation

/// Where the Provider usage screen reads from (#504).
enum ProviderUsageSource: Sendable, Equatable {
    /// The offline fixture projection, chosen by the launch argument
    /// `-providerUsageFixture full|nodata|sparse|empty|failing` (no network, the saved host untouched).
    case fixture(ProviderUsageFixture.Variant)
    /// The hub's `GET /api/v2/providers` (hub #3796), read through the saved host's `RemoteClient`.
    case hub

    static let launchKey = "providerUsageFixture"

    static func current(_ defaults: UserDefaults = .standard) -> ProviderUsageSource {
        parse(defaults.string(forKey: launchKey))
    }

    /// `nil` or an unknown word is the hub; a known word is that fixture.
    static func parse(_ raw: String?) -> ProviderUsageSource {
        guard let raw, let variant = ProviderUsageFixture.Variant(
            rawValue: raw.trimmingCharacters(in: .whitespaces).lowercased())
        else { return .hub }
        return .fixture(variant)
    }

    /// A fixture was chosen at launch: the screen opens offline as the root.
    var isFixture: Bool { self != .hub }
}

/// The Provider usage screen's data: the last good projection with its age, the
/// failure that kept it from refreshing (#350's `Freshness`), and the read
/// that runs on open, on pull-to-refresh and every minute while shown.
@MainActor
@Observable
final class ProviderUsageStore {
    /// How often the shown screen reads again by itself while all is well.
    static let every: Duration = .seconds(60)

    /// The last projection; kept while a refresh fails.
    private(set) var usage: ProviderUsage?
    private(set) var freshness = Freshness()
    private(set) var loading = false
    var error: RemoteError? { freshness.error }

    /// A fixture or test read; nil reads the hub through `client`.
    @ObservationIgnored private let load: (@Sendable () async throws -> ProviderUsage)?
    @ObservationIgnored var now: @MainActor () -> Date
    /// The saved host's client, asked at every read: the host can change.
    @ObservationIgnored var client: @MainActor () -> RemoteClient?

    init(
        load: (@Sendable () async throws -> ProviderUsage)? = nil,
        client: @escaping @MainActor () -> RemoteClient? = { nil },
        now: @escaping @MainActor () -> Date = { Date() }
    ) {
        self.load = load
        self.client = client
        self.now = now
    }

    convenience init(
        source: ProviderUsageSource, client: @escaping @MainActor () -> RemoteClient? = { nil },
        now: @escaping @MainActor () -> Date = { Date() }
    ) {
        self.init(load: Self.loader(source), client: client, now: now)
    }

    private static func loader(_ source: ProviderUsageSource) -> (@Sendable () async throws -> ProviderUsage)? {
        switch source {
        case .fixture(.failing):
            return { throw RemoteError.server(status: 500, code: "fixture_failing") }
        case .fixture(let variant):
            return { try ProviderUsageFixture.decoded(variant, now: Date()) }
        case .hub:
            return nil
        }
    }

    /// One read: the fixture, or `GET /api/v2/providers` on the saved host.
    private func read() async throws -> ProviderUsage {
        if let load { return try await load() }
        guard let client = client() else { throw RemoteError.notConfigured }
        return try await client.providers()
    }

    /// Reads the projection now. A failure keeps the last good one on screen; a
    /// read given up because the screen went away changes nothing.
    func refresh() async {
        loading = true
        defer { loading = false }
        do {
            usage = try await read()
            freshness.succeeded(at: now())
        } catch {
            let failure: RemoteError =
                error as? RemoteError ?? (error is CancellationError ? .cancelled : .invalidResponse("provider usage"))
            freshness.failed(failure, at: now())
        }
    }

    /// Forgets everything (the host changed).
    func reset() {
        usage = nil
        freshness = Freshness()
    }

    /// While the screen is shown: read at once unless the last read is only
    /// seconds old, then every minute, or on the backoff after a failure.
    func keepFresh(reuseFor: TimeInterval = AppModel.reuseFor) async {
        while !Task.isCancelled {
            if freshness.isStale(reuseFor, now: now()) { await refresh() }
            do {
                try await Task.sleep(for: freshness.wait(every: Self.every, now: now()))
            } catch {
                return
            }
        }
    }
}
