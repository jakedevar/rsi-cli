import Foundation

/// A fixture projection for the Provider usage screen (#504), in the shape of
/// `GET /api/v2/providers` (hub #3796). The contract's own bodies are under
/// remote/contracts/v2/fixtures/providers_*.json; these are the same shape
/// with times written relative to `now`, so the countdowns read the same on
/// every run, for the launch argument `-providerUsageFixture` and the
/// screenshots. They carry what a later hub may add (`future_field`, a
/// `credits` block) to show it is ignored.
enum ProviderUsageFixture {
    enum Variant: String, CaseIterable, Sendable {
        /// Four providers: windows near and past limits, spend, a stale one, one with only spend, one with nothing.
        case full
        /// One provider, one window, using overage.
        case sparse
        /// Providers the hub knows but has no readings or spend for yet (reason codes beside null blocks).
        case nodata
        /// No provider listed.
        case empty
        /// The hub answers 5xx: the screen's failure state.
        case failing
    }

    static func data(_ variant: Variant, now: Date) -> Data {
        Data(json(variant, now: now).utf8)
    }

    static func decoded(_ variant: Variant, now: Date) throws -> ProviderUsage {
        try JSONDecoder().decode(ProviderUsage.self, from: data(variant, now: now))
    }

    private static func iso(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.string(from: date)
    }

    /// A provider the hub has never observed and has no spend for.
    private static func nothing(_ name: String, spend: String = "warming") -> String {
        """
        {"provider":"\(name)","status":null,"binding_window":null,"overage_status":null,
         "is_using_overage":false,"observed_at":null,"stale_reason":null,
         "windows":[],"windows_reason":"no_observation","windows_truncated":false,
         "spend":null,"spend_reason":"\(spend)","models":[],"models_truncated":false}
        """
    }

    static func json(_ variant: Variant, now: Date) -> String {
        let observed = iso(now.addingTimeInterval(-20))
        let tail = #""providers_truncated":false,"windows_incomplete":false"#
        switch variant {
        case .empty, .failing:
            return #"{"observed_at":"\#(observed)","providers":[],\#(tail)}"#
        case .nodata:
            return """
                {"observed_at":"\(observed)","providers":[
                  \(nothing("OpenRouter")),
                  \(nothing("Bedrock", spend: "unavailable")),
                  \(nothing("Local", spend: "stale"))
                ],\(tail)}
                """
        case .sparse:
            return """
                {"observed_at":"\(observed)","providers":[
                  {"provider":"Claude","status":"allowed","binding_window":"five_hour",
                   "overage_status":"allowed","is_using_overage":true,"observed_at":"\(observed)","stale_reason":null,
                   "windows":[{"key":"five_hour","utilization":1.0,"resets_at":"\(iso(now.addingTimeInterval(25 * 60)))","resets_at_epoch":null}],
                   "windows_reason":null,"windows_truncated":false,
                   "spend":null,"spend_reason":"truncated","models":[],"models_truncated":false}
                ],\(tail)}
                """
        case .full:
            let claude5h = now.addingTimeInterval(2 * 3600 + 14 * 60)
            let claude7d = now.addingTimeInterval(3 * 86_400 + 4 * 3600)
            let codexPrimary = now.addingTimeInterval(47 * 60)
            let codexWeekly = now.addingTimeInterval(5 * 86_400)
            func epoch(_ date: Date) -> Int { Int(date.timeIntervalSince1970) }
            return """
                {"observed_at":"\(observed)",\(tail),"future_field":{"ignored":true},"providers":[
                  {"provider":"Claude","status":"allowed","binding_window":"five_hour",
                   "overage_status":"rejected","is_using_overage":false,"observed_at":"\(observed)","stale_reason":null,
                   "windows":[
                     {"key":"five_hour","utilization":0.42,"resets_at":"\(iso(claude5h))","resets_at_epoch":\(epoch(claude5h))},
                     {"key":"seven_day","utilization":0.81,"resets_at":"\(iso(claude7d))","resets_at_epoch":\(epoch(claude7d))}],
                   "windows_reason":null,"windows_truncated":false,
                   "spend":{"cost_1h_usd":3.2,"cost_24h_usd":41.75},"spend_reason":null,
                   "models":[
                     {"model":"claude-opus-5","cost_1h_usd":2.6,"cost_24h_usd":33.1},
                     {"model":"claude-sonnet-5-5","cost_1h_usd":0.6,"cost_24h_usd":8.65}],
                   "models_truncated":false,"future_field":"ignored"},
                  {"provider":"Codex","status":"allowed_warning","binding_window":"primary",
                   "overage_status":null,"is_using_overage":false,
                   "observed_at":"\(iso(now.addingTimeInterval(-3 * 3600)))","stale_reason":"probe_timeout",
                   "windows":[
                     {"key":"primary","utilization":0.93,"resets_at":null,"resets_at_epoch":\(epoch(codexPrimary))},
                     {"key":"secondary","utilization":0.35,"resets_at":"\(iso(codexWeekly))","resets_at_epoch":\(epoch(codexWeekly))}],
                   "windows_reason":null,"windows_truncated":false,
                   "spend":{"cost_1h_usd":0.0,"cost_24h_usd":12.4},"spend_reason":null,
                   "models":[],"models_truncated":true},
                  {"provider":"OpenRouter","status":null,"binding_window":null,"overage_status":null,
                   "is_using_overage":false,"observed_at":null,"stale_reason":null,
                   "windows":[],"windows_reason":"no_observation","windows_truncated":false,
                   "spend":{"cost_1h_usd":0.4,"cost_24h_usd":6.1},"spend_reason":null,
                   "models":[{"model":"qwen3-coder","cost_1h_usd":0.4,"cost_24h_usd":6.1}],"models_truncated":false,
                   "credits":{"remaining_usd":12.34}},
                  \(nothing("Bedrock"))
                ]}
                """
        }
    }
}
