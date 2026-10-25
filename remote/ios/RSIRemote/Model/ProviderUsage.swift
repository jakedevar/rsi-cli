import Foundation

// Provider usage (#504): every provider's plan windows with utilisation and
// reset time, overage status and recent spend.
//
// The wire type is `GET /api/v2/providers` (hub #3796, docs/rsi-remote/
// contracts-v2.md "### GET /api/v2/providers", fixtures providers_*.json). It
// is decoded tolerantly: every field is optional, a null is "not known" (the
// reason code beside a block says why), unknown fields and blocks are
// ignored, and a broken provider, window or model is dropped instead of
// failing the read. The hub serves no credit balances: the screen shows plan
// windows and spend only.

/// The projection: one entry per provider the daemon knows, canonical order.
struct ProviderUsage: Decodable, Sendable, Equatable {
    /// When the hub built it (RFC3339).
    let observedAt: String?
    let providers: [ProviderUsageEntry]
    /// The hub listed fewer providers than it has.
    let providersTruncated: Bool
    /// The hub's rate-limit read hit its row cap: some windows may be missing.
    let windowsIncomplete: Bool
    /// Entries the app could not read at all (no provider name); never fatal.
    let unreadableProviders: Int

    enum CodingKeys: String, CodingKey {
        case observedAt = "observed_at"
        case providers
        case providersTruncated = "providers_truncated"
        case windowsIncomplete = "windows_incomplete"
    }

    init(observedAt: String? = nil, providers: [ProviderUsageEntry] = [], providersTruncated: Bool = false,
         windowsIncomplete: Bool = false, unreadableProviders: Int = 0) {
        self.observedAt = observedAt
        self.providers = providers
        self.providersTruncated = providersTruncated
        self.windowsIncomplete = windowsIncomplete
        self.unreadableProviders = unreadableProviders
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        observedAt = (try? c.decodeIfPresent(String.self, forKey: .observedAt)) ?? nil
        providersTruncated = (try? c.decodeIfPresent(Bool.self, forKey: .providersTruncated)) ?? false
        windowsIncomplete = (try? c.decodeIfPresent(Bool.self, forKey: .windowsIncomplete)) ?? false
        let lossy = (try? c.decodeIfPresent([Lossy<ProviderUsageEntry>].self, forKey: .providers)) ?? []
        providers = lossy.compactMap(\.value)
        unreadableProviders = lossy.count - providers.count
    }
}

/// One provider. `provider` is the only required field.
struct ProviderUsageEntry: Decodable, Sendable, Equatable, Identifiable {
    /// `Claude`, `Codex`, `OpenRouter`, `Bedrock`, ... (the daemon's provider name).
    let provider: String
    /// The provider's own verdict for its last request (`allowed`, ...).
    let status: String?
    /// The window the provider itself calls binding (`five_hour`, ...).
    let bindingWindow: String?
    /// `allowed`, `rejected`, ...
    let overageStatus: String?
    let isUsingOverage: Bool
    /// When the hub last saw this provider's numbers (RFC3339).
    let observedAt: String?
    /// Set when the latest refresh failed: the numbers are the last known ones.
    let staleReason: String?
    let windows: [UsageWindow]
    /// Why `windows` is empty (`no_observation`).
    let windowsReason: String?
    let windowsTruncated: Bool
    /// Cost over the last hour and 24 hours; nil with `spendReason` beside it.
    let spend: UsageSpend?
    /// Why `spend` is nil: `warming`, `unavailable`, `stale` or `truncated`.
    let spendReason: String?
    /// Per-model cost, largest 24 h first (the hub bounds it at 8).
    let models: [UsageModelSpend]
    let modelsTruncated: Bool

    var id: String { provider }

    enum CodingKeys: String, CodingKey {
        case provider, status, windows, spend, models
        case bindingWindow = "binding_window"
        case overageStatus = "overage_status"
        case isUsingOverage = "is_using_overage"
        case observedAt = "observed_at"
        case staleReason = "stale_reason"
        case windowsReason = "windows_reason"
        case windowsTruncated = "windows_truncated"
        case spendReason = "spend_reason"
        case modelsTruncated = "models_truncated"
    }

    init(provider: String, status: String? = nil, bindingWindow: String? = nil, overageStatus: String? = nil,
         isUsingOverage: Bool = false, observedAt: String? = nil, staleReason: String? = nil,
         windows: [UsageWindow] = [], windowsReason: String? = nil, windowsTruncated: Bool = false,
         spend: UsageSpend? = nil, spendReason: String? = nil,
         models: [UsageModelSpend] = [], modelsTruncated: Bool = false) {
        self.provider = provider
        self.status = status
        self.bindingWindow = bindingWindow
        self.overageStatus = overageStatus
        self.isUsingOverage = isUsingOverage
        self.observedAt = observedAt
        self.staleReason = staleReason
        self.windows = windows
        self.windowsReason = windowsReason
        self.windowsTruncated = windowsTruncated
        self.spend = spend
        self.spendReason = spendReason
        self.models = models
        self.modelsTruncated = modelsTruncated
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let name = try c.decode(String.self, forKey: .provider).trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else {
            throw DecodingError.dataCorruptedError(forKey: .provider, in: c, debugDescription: "empty provider")
        }
        provider = name
        status = Self.text(c, .status)
        bindingWindow = Self.text(c, .bindingWindow)
        overageStatus = Self.text(c, .overageStatus)
        isUsingOverage = (try? c.decodeIfPresent(Bool.self, forKey: .isUsingOverage)) ?? false
        observedAt = Self.text(c, .observedAt)
        staleReason = Self.text(c, .staleReason)
        windows = ((try? c.decodeIfPresent([Lossy<UsageWindow>].self, forKey: .windows)) ?? []).compactMap(\.value)
        windowsReason = Self.text(c, .windowsReason)
        windowsTruncated = (try? c.decodeIfPresent(Bool.self, forKey: .windowsTruncated)) ?? false
        let spend = (try? c.decodeIfPresent(UsageSpend.self, forKey: .spend)) ?? nil
        self.spend = spend?.isEmpty == true ? nil : spend
        spendReason = Self.text(c, .spendReason)
        models = ((try? c.decodeIfPresent([Lossy<UsageModelSpend>].self, forKey: .models)) ?? []).compactMap(\.value)
        modelsTruncated = (try? c.decodeIfPresent(Bool.self, forKey: .modelsTruncated)) ?? false
    }

    private static func text(_ c: KeyedDecodingContainer<CodingKeys>, _ key: CodingKeys) -> String? {
        guard let value = (try? c.decodeIfPresent(String.self, forKey: key)) ?? nil else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? nil : trimmed
    }

    /// Anything to show besides the provider's name.
    var hasData: Bool { !windows.isEmpty || spend != nil || !models.isEmpty }
}

/// One plan window: the rolling five hours, the week, ...
struct UsageWindow: Decodable, Sendable, Equatable, Identifiable {
    /// The provider's key (`five_hour`, `seven_day`, ...): not an enum, a new
    /// window must show, not vanish.
    let key: String
    /// The fraction consumed (0...1 from the hub; more than 1 is kept).
    let utilization: Double?
    let resetsAt: Date?

    var id: String { key }

    enum CodingKeys: String, CodingKey {
        case key, utilization
        case resetsAt = "resets_at"
        case resetsAtEpoch = "resets_at_epoch"
    }

    init(key: String, utilization: Double?, resetsAt: Date?) {
        self.key = key
        self.utilization = utilization
        self.resetsAt = resetsAt
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        guard let name = (try? c.decodeIfPresent(String.self, forKey: .key)) ?? nil,
            !name.trimmingCharacters(in: .whitespaces).isEmpty
        else {
            throw DecodingError.dataCorruptedError(forKey: .key, in: c, debugDescription: "window without a key")
        }
        key = name
        let raw = (try? c.decodeIfPresent(Double.self, forKey: .utilization)) ?? nil
        utilization = raw.flatMap { $0.isFinite && $0 >= 0 ? $0 : nil }
        // The reset time: RFC3339 `resets_at`, else the same instant in epoch
        // seconds (`resets_at_epoch`); both null when the provider gave none.
        if let text = (try? c.decodeIfPresent(String.self, forKey: .resetsAt)) ?? nil, let parsed = parseWireTime(text) {
            resetsAt = parsed
        } else if let epoch = (try? c.decodeIfPresent(Double.self, forKey: .resetsAtEpoch)) ?? nil {
            resetsAt = Self.date(epoch: epoch)
        } else {
            resetsAt = nil
        }
    }

    static func date(epoch: Double) -> Date? {
        guard epoch.isFinite, epoch > 0 else { return nil }
        return Date(timeIntervalSince1970: epoch)
    }
}

/// Cost over the last hour and 24 hours, USD. The hub sums each invocation's
/// own cost, so for subscription plans it is an API-equivalent estimate, not
/// a bill.
struct UsageSpend: Decodable, Sendable, Equatable {
    let cost1hUSD: Double?
    let cost24hUSD: Double?

    enum CodingKeys: String, CodingKey {
        case cost1hUSD = "cost_1h_usd"
        case cost24hUSD = "cost_24h_usd"
    }

    init(cost1hUSD: Double? = nil, cost24hUSD: Double? = nil) {
        self.cost1hUSD = cost1hUSD
        self.cost24hUSD = cost24hUSD
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        cost1hUSD = ((try? c.decodeIfPresent(Double.self, forKey: .cost1hUSD)) ?? nil).flatMap { $0.isFinite ? $0 : nil }
        cost24hUSD = ((try? c.decodeIfPresent(Double.self, forKey: .cost24hUSD)) ?? nil).flatMap { $0.isFinite ? $0 : nil }
    }

    var isEmpty: Bool { cost1hUSD == nil && cost24hUSD == nil }
}

/// One model's recent cost.
struct UsageModelSpend: Decodable, Sendable, Equatable, Identifiable {
    let model: String
    let spend: UsageSpend

    var id: String { model }

    enum CodingKeys: String, CodingKey { case model }

    init(model: String, spend: UsageSpend) {
        self.model = model
        self.spend = spend
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        model = try c.decode(String.self, forKey: .model)
        spend = try UsageSpend(from: decoder)
    }
}

/// An array whose broken elements are dropped instead of failing the whole read.
struct Lossy<T: Decodable>: Decodable {
    let value: T?

    init(from decoder: Decoder) throws {
        value = try? T(from: decoder)
    }
}

// MARK: - View model

/// How a window, status or overage line reads: calm, near a limit, or at it.
enum UsageTone: Sendable, Equatable {
    case idle, ok, warn, error
}

/// A window as the screen shows it, at one moment (`now` drives the countdown).
struct UsageWindowRow: Sendable, Equatable, Identifiable {
    let id: String
    /// `5-hour`, `7-day`, ...
    let label: String
    /// 0...1 for the bar; nil when the provider gave no reading.
    let fraction: Double?
    /// `42%`, or `No reading`.
    let percentText: String
    /// `Resets in 2 h 14 min`, `Reset time not reported`, ...
    let resetText: String
    let tone: UsageTone
    /// The provider counts this window against its limit right now.
    let isBinding: Bool
    /// The window's reset time has passed: its reading is from before the reset.
    let isExpired: Bool

    /// Utilisation at or above these fractions turns the bar warning / error.
    static let warnAt = 0.75
    static let errorAt = 0.9

    init(_ window: UsageWindow, binding: Bool, now: Date) {
        id = window.key
        label = usageWindowLabel(window.key)
        isBinding = binding
        isExpired = window.resetsAt.map { $0 <= now } ?? false
        if let value = window.utilization {
            fraction = min(1, value)
            percentText = "\(Int((value * 100).rounded()))%"
            tone = value >= Self.errorAt ? .error : value >= Self.warnAt ? .warn : .ok
        } else {
            fraction = nil
            percentText = "No reading"
            tone = .idle
        }
        if let resets = window.resetsAt {
            resetText = resets > now
                ? "Resets in \(resetCountdown(resets.timeIntervalSince(now)))"
                : "Reset \(resetCountdown(now.timeIntervalSince(resets))) ago; waiting for a new reading"
        } else {
            resetText = "Reset time not reported"
        }
    }
}

/// One provider as the screen shows it, at one moment.
struct ProviderUsageRow: Sendable, Equatable, Identifiable {
    let id: String
    let name: String
    /// `Within limits`, `Near limit`, `Limit reached`, ...
    let status: String?
    let statusTone: UsageTone
    let windows: [UsageWindowRow]
    /// `Overage not available`, `Using overage`, ...
    let overage: String?
    let overageTone: UsageTone
    /// `$3.20 last hour · $41.75 last 24 h`.
    let spend: String?
    /// What the spend figures are: `API-equivalent estimate` (subscription
    /// plans are priced at list price, not billed), shown beside them.
    let spendLabel: String?
    /// Why a block is missing or cut short, in words (`windows_reason`,
    /// `spend_reason`, `*_truncated`).
    let reasonNotes: [String]
    /// Per model: (model, `$1.10 last hour · $20.00 last 24 h`).
    let modelSpend: [ModelSpendRow]
    /// Why the numbers may be old, or how old they are.
    let staleNote: String?
    /// The provider has nothing to show: the screen says so.
    let hasData: Bool

    struct ModelSpendRow: Sendable, Equatable, Identifiable {
        let model: String
        let text: String
        var id: String { model }
    }

    /// Spend is the sum of each call's own cost: for a subscription plan that
    /// is the list price of the same calls, not money charged (hub #3533
    /// relabels it later).
    static let spendLabelText = "API-equivalent estimate"

    /// Provider numbers older than this show their age.
    static let showAgeAfter: TimeInterval = 600

    init(_ entry: ProviderUsageEntry, now: Date) {
        id = entry.id
        name = entry.provider
        (status, statusTone) = usageStatus(entry.status)
        windows = entry.windows.map {
            UsageWindowRow($0, binding: entry.bindingWindow == $0.key, now: now)
        }
        (overage, overageTone) = usageOverage(status: entry.overageStatus, using: entry.isUsingOverage)
        spend = entry.spend.flatMap(usageSpendText)
        modelSpend = entry.models.compactMap { model in
            usageSpendText(model.spend).map { ModelSpendRow(model: model.model, text: $0) }
        }
        hasData = entry.hasData
        spendLabel = spend != nil || !modelSpend.isEmpty ? Self.spendLabelText : nil
        var notes: [String] = []
        if let reason = entry.windowsReason { notes.append(usageWindowsReasonText(reason)) }
        if entry.windowsTruncated { notes.append("Only some plan windows are listed.") }
        if let reason = entry.spendReason { notes.append(usageSpendReasonText(reason)) }
        if entry.modelsTruncated { notes.append("Only the largest models are listed.") }
        reasonNotes = notes
        if let reason = entry.staleReason {
            staleNote = "Last known values; the latest refresh failed (\(reason.replacingOccurrences(of: "_", with: " ")))."
        } else if let seen = entry.observedAt.flatMap(parseWireTime),
            now.timeIntervalSince(seen) >= Self.showAgeAfter
        {
            staleNote = "Last seen \(ageText(now.timeIntervalSince(seen)))."
        } else {
            staleNote = nil
        }
    }

    /// What the screen says under a provider with nothing to show.
    var noDataText: String { "No usage data from \(name) yet." }
}

extension ProviderUsage {
    func rows(now: Date) -> [ProviderUsageRow] { providers.map { ProviderUsageRow($0, now: now) } }

    /// `4 providers · 2 with data`, or that none is configured.
    var summary: String {
        guard !providers.isEmpty else { return "No providers" }
        let withData = providers.filter(\.hasData).count
        return "\(providers.count) provider\(providers.count == 1 ? "" : "s") · \(withData) with data"
    }

    /// The note when the hub or the app left some providers out.
    var incompleteNote: String? {
        var parts: [String] = []
        if providersTruncated { parts.append("The hub lists only some providers.") }
        if windowsIncomplete { parts.append("Some plan-window readings may be missing.") }
        if unreadableProviders > 0 {
            parts.append("\(unreadableProviders) provider\(unreadableProviders == 1 ? "" : "s") could not be read.")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " ")
    }
}

// MARK: - Words

/// `five_hour` → `5-hour`; an unknown key reads as plain words (`seven_day_opus` → `7-day Opus`).
func usageWindowLabel(_ key: String) -> String {
    let known: [String: String] = [
        "five_hour": "5-hour", "seven_day": "7-day", "seven_day_opus": "7-day Opus",
        "seven_day_sonnet": "7-day Sonnet", "primary": "Primary", "secondary": "Secondary",
        "hourly": "Hourly", "daily": "Daily", "weekly": "Weekly", "monthly": "Monthly",
    ]
    let lower = key.lowercased()
    if let label = known[lower] { return label }
    let words = lower.split(whereSeparator: { $0 == "_" || $0 == "-" || $0 == " " }).map(String.init)
    guard !words.isEmpty else { return key }
    let text = words.joined(separator: " ")
    return text.prefix(1).uppercased() + text.dropFirst()
}

/// `2 h 14 min`, `3 d 4 h`, `45 min`, `under a minute`.
func resetCountdown(_ seconds: TimeInterval) -> String {
    let whole = max(0, Int(seconds))
    switch whole {
    case ..<60: return "under a minute"
    case ..<3600: return "\(whole / 60) min"
    case ..<86_400:
        let minutes = (whole % 3600) / 60
        return minutes == 0 ? "\(whole / 3600) h" : "\(whole / 3600) h \(minutes) min"
    default:
        let hours = (whole % 86_400) / 3600
        return hours == 0 ? "\(whole / 86_400) d" : "\(whole / 86_400) d \(hours) h"
    }
}

/// The provider's verdict in words.
func usageStatus(_ raw: String?) -> (String?, UsageTone) {
    guard let raw else { return (nil, .idle) }
    switch raw.lowercased() {
    case "allowed": return ("Within limits", .ok)
    case "allowed_warning", "warning": return ("Near limit", .warn)
    case "rejected", "limit_reached", "exceeded", "blocked": return ("Limit reached", .error)
    default: return (usageWindowLabel(raw), .idle)
    }
}

/// The overage line: using it is a warning; otherwise whether it is available.
func usageOverage(status: String?, using: Bool) -> (String?, UsageTone) {
    if using { return ("Using overage", .warn) }
    guard let status else { return (nil, .idle) }
    switch status.lowercased() {
    case "allowed": return ("Overage available", .idle)
    case "rejected", "disabled", "unavailable": return ("Overage not available", .idle)
    case "allowed_warning": return ("Overage nearly used", .warn)
    default: return ("Overage: \(usageWindowLabel(status).lowercased())", .idle)
    }
}

/// `windows_reason` as words: why a provider has no plan windows.
func usageWindowsReasonText(_ code: String) -> String {
    switch code.lowercased() {
    case "no_observation": return "No plan-window reading yet."
    default: return "Plan windows: \(code.replacingOccurrences(of: "_", with: " "))."
    }
}

/// `spend_reason` as words: why a provider has no spend figure.
func usageSpendReasonText(_ code: String) -> String {
    switch code.lowercased() {
    case "warming": return "Spend is still warming up."
    case "unavailable": return "Spend is unavailable right now."
    case "stale": return "Spend figures are out of date."
    case "truncated": return "Spend is incomplete: the usage read hit its cap."
    default: return "Spend: \(code.replacingOccurrences(of: "_", with: " "))."
    }
}

/// `$1,234`, `$12.34`, `<$0.01`; negative amounts keep their sign.
func usd(_ value: Double) -> String {
    if value > 0, value < 0.01 { return "<$0.01" }
    let formatter = NumberFormatter()
    formatter.locale = Locale(identifier: "en_US")
    formatter.numberStyle = .currency
    formatter.currencyCode = "USD"
    let digits = abs(value) >= 1000 ? 0 : 2
    formatter.minimumFractionDigits = digits
    formatter.maximumFractionDigits = digits
    return formatter.string(from: NSNumber(value: value)) ?? String(format: "$%.2f", value)
}

/// `$3.20 last hour · $41.75 last 24 h`; nil when the hub gave neither.
func usageSpendText(_ spend: UsageSpend) -> String? {
    var parts: [String] = []
    if let hour = spend.cost1hUSD { parts.append("\(usd(hour)) last hour") }
    if let day = spend.cost24hUSD { parts.append("\(usd(day)) last 24 h") }
    return parts.isEmpty ? nil : parts.joined(separator: " · ")
}
