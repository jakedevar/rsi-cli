import Foundation

// Drives the app's own AppModel and RemoteClient (compiled from
// remote/ios/RSIRemote for macOS by run.sh) against fake_gateway.py, one fault
// at a time, and prints what the operator would see. #350.

/// Accepts the fake gateway's self-signed certificate on localhost only.
final class TrustLocalhost: NSObject, URLSessionDelegate, @unchecked Sendable {
    func urlSession(
        _ session: URLSession, didReceive challenge: URLAuthenticationChallenge
    ) async -> (URLSession.AuthChallengeDisposition, URLCredential?) {
        let space = challenge.protectionSpace
        if space.authenticationMethod == NSURLAuthenticationMethodServerTrust, space.host == "localhost",
            let trust = space.serverTrust
        {
            return (.useCredential, URLCredential(trust: trust))
        }
        return (.performDefaultHandling, nil)
    }
}

/// The gateway route of a path, by its daemon method (never ids).
func routeName(_ path: String) -> String {
    let parts = path.split(separator: "/").map(String.init)
    switch (parts.first, parts.count, parts.last) {
    case ("auth", _, let last?): return "auth/\(last)"
    case ("api", 2...3, "info"): return "GetInfoV1"
    case ("api", 3, "projects") where parts[1] == "v1": return "ListProjectsV1"
    case ("api", 3, "projects"): return "ListManagerSeatsV2"
    case ("api", 3, "dashboard"): return "DashboardV2"
    case ("api", 5, "sessions"): return "ListSessionsV1"
    case ("api", 6, _): return "GetSessionV1"
    case ("api", 7, "history"): return "GetHistoryPageV1"
    case ("api", 7, "decisions"): return "GetDecisionsV1"
    case ("api", 7, "decision-targets"): return "GetDecisionTargetsV1"
    case (_, _, "conversation"): return "ConversationV2"
    default: return "other"
    }
}

func utcNow() -> String {
    let formatter = ISO8601DateFormatter()
    formatter.formatOptions = [.withInternetDateTime]
    return formatter.string(from: Date())
}

/// Every request the soak makes: a line for each one that is not a 200, and
/// counts by route and outcome.
final class RequestLog: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private var counts: [String: Int] = [:]

    var summary: [String] {
        lock.withLock { counts.sorted { $0.key < $1.key }.map { "\($0.key) x\($0.value)" } }
    }

    private func note(_ key: String) {
        lock.withLock { counts[key, default: 0] += 1 }
    }

    func urlSession(
        _ session: URLSession, task: URLSessionTask, didFinishCollecting metrics: URLSessionTaskMetrics
    ) {
        guard let status = (task.response as? HTTPURLResponse)?.statusCode else { return }
        let route = routeName(task.originalRequest?.url?.path ?? "")
        note("\(route) \(status)")
        if status != 200 {
            let ms = Int(metrics.taskInterval.duration * 1000)
            print("  \(utcNow()) \(route) -> \(status) (\(ms) ms)")
        }
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: (any Error)?) {
        guard let error, task.response == nil else { return }
        let route = routeName(task.originalRequest?.url?.path ?? "")
        let code = (error as? URLError)?.code
        if code == .cancelled { return }
        note("\(route) error \(code?.rawValue ?? 0)")
        print("  \(utcNow()) \(route) -> transport error \(unreachableReason(error) ?? "\(code?.rawValue ?? 0)")")
    }
}

/// Read a real gateway the way the app does, read-only, one refresh cycle every
/// 30 s for `minutes`: the Dashboard, the Waiting screen (which reads the
/// projects and session lists again) and each waiting row's answer forms.
@MainActor
func soak(host: String, minutes: Double) async {
    let log = RequestLog()
    let suite = "rsi-reliability-soak-\(UUID().uuidString)"
    let model = AppModel(defaults: UserDefaults(suiteName: suite)!) { host in
        RemoteClient(
            host: host,
            session: URLSession(configuration: RemoteClient.configuration(), delegate: log, delegateQueue: nil))
    }
    model.setHost(host)
    let end = Date().addingTimeInterval(minutes * 60)
    var cycle = 0
    print("soak: \(utcNow()) start, one cycle per 30 s for \(minutes) min")
    while Date() < end {
        cycle += 1
        let started = Date()
        await model.refreshDashboard()
        await model.refreshWaiting()
        let client = model.client!
        let targetFailures = await withTaskGroup(of: String?.self) { group in
            for item in model.waiting {
                group.addTask {
                    let outcome = await client.decisionTargets(project: item.project.id, session: item.session.id)
                    if case .failed(let error) = outcome { return describe(error) }
                    return nil
                }
            }
            var failures: [String] = []
            for await failure in group { if let failure { failures.append(failure) } }
            return failures
        }
        var shown: [String] = []
        if let error = model.dashboardError { shown.append("dashboard: \(describe(error))") }
        if let error = model.projectsError { shown.append("projects: \(describe(error))") }
        if let error = model.waitingError { shown.append("waiting: \(describe(error))") }
        let rowErrors = model.projects.filter { $0.error != nil }.count
        if rowErrors > 0 { shown.append("\(rowErrors) project rows not refreshed") }
        let itemErrors = model.waiting.filter { $0.error != nil }.count
        if itemErrors > 0 { shown.append("\(itemErrors) waiting rows not refreshed") }
        if !targetFailures.isEmpty { shown.append("\(targetFailures.count) answer forms failed") }
        let partial = partialCount(model)
        let took = String(format: "%.1f", Date().timeIntervalSince(started))
        print(
            "cycle \(cycle) \(utcNow()) \(took)s: \(model.projects.count) projects (\(partial) partial),"
                + " \(model.waiting.count) waiting; on screen: \(shown.isEmpty ? "all fresh" : shown.joined(separator: "; "))")
        try? await Task.sleep(for: .seconds(max(1, 30 - Date().timeIntervalSince(started))))
    }
    print("soak: \(utcNow()) done; requests by route and outcome:")
    for line in log.summary { print("  \(line)") }
}

struct LogEntry: Decodable {
    let t: Double
    let method: String
    let path: String
    let status: Int
    let note: String
}

@MainActor
enum Harness {
    static let host = ProcessInfo.processInfo.environment["RSI_FAKE_GATEWAY"] ?? "localhost:8443"
    static let control = URLSession(
        configuration: .ephemeral, delegate: TrustLocalhost(), delegateQueue: nil)

    static func makeModel() -> AppModel {
        let suite = "rsi-reliability-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        let model = AppModel(defaults: defaults) { host in
            // Production settings, plus h2-like concurrency (tailscale serve speaks HTTP/2).
            let configuration = RemoteClient.configuration()
            configuration.httpMaximumConnectionsPerHost = 16
            let session = URLSession(
                configuration: configuration, delegate: TrustLocalhost(), delegateQueue: nil)
            return RemoteClient(host: host, session: session)
        }
        model.setHost(host)
        return model
    }

    static func faults(_ changes: [String: Any]) async throws {
        var request = URLRequest(url: URL(string: "https://\(host)/__control")!)
        request.httpMethod = "POST"
        request.httpBody = try JSONSerialization.data(withJSONObject: changes)
        let (_, response) = try await control.data(for: request)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else {
            throw URLError(.badServerResponse)
        }
    }

    static func gatewayLog() async throws -> [LogEntry] {
        struct Log: Decodable { let entries: [LogEntry] }
        let (data, _) = try await control.data(from: URL(string: "https://\(host)/__log")!)
        return try JSONDecoder().decode(Log.self, from: data).entries
    }

    /// One line of gateway counters: requests by status and the notable notes.
    static func summary(_ log: [LogEntry]) -> String {
        let real = log.filter { !$0.path.hasPrefix("/__") }
        let boots = real.filter { $0.path == "/auth/bootstrap" }.count
        var byStatus: [Int: Int] = [:]
        for entry in real { byStatus[entry.status, default: 0] += 1 }
        var notes: [String: Int] = [:]
        for entry in real where !entry.note.isEmpty { notes[entry.note, default: 0] += 1 }
        let statuses = byStatus.keys.sorted().map { "\($0 == 0 ? "drop" : String($0))x\(byStatus[$0]!)" }
        let named = notes.keys.sorted().map { "\($0)x\(notes[$0]!)" }
        return "requests=\(real.count) bootstraps=\(boots) [\(statuses.joined(separator: " "))]"
            + (named.isEmpty ? "" : " notes[\(named.joined(separator: " "))]")
    }

    /// What the Projects screen shows.
    static func projectsView(_ model: AppModel) -> String {
        if let error = model.projectsError, model.projects.isEmpty {
            return "full-screen error: \"\(error.message)\""
        }
        let rowErrors = Set(model.projects.compactMap(\.error))
        var text = "\(model.projects.count) projects"
        if !rowErrors.isEmpty {
            text += ", \(model.projects.filter { $0.error != nil }.count) row errors \(rowErrors.sorted())"
        }
        if let error = model.projectsError {
            #if RELIABILITY_STATUS
                text += ", refresh failed: \"\(error.message)\""
            #else
                text += ", refresh failed (not shown on screen): \"\(error.message)\""
            #endif
        }
        return text + extraStatus(model)
    }

    static func waitingView(_ model: AppModel) -> String {
        if let error = model.waitingError, model.waiting.isEmpty {
            return "full-screen error: \"\(error.message)\""
        }
        let itemErrors = Set(model.waiting.compactMap(\.error))
        var text = "\(model.waiting.count) waiting items"
        if !itemErrors.isEmpty {
            text += ", \(model.waiting.filter { $0.error != nil }.count) item errors \(itemErrors.sorted())"
        }
        return text
    }

    static func dashboardView(_ model: AppModel) -> String {
        switch (model.dashboard, model.dashboardError) {
        case (nil, let error?): "full-screen error: \"\(error.message)\""
        case (_?, let error?): "data + warning line: \"\(error.message)\"" + extraDashboard(model)
        case (_?, nil): "data, no error"
        case (nil, nil): "nothing"
        }
    }

    /// Status lines only the fixed app has (compiled with -D RELIABILITY_STATUS).
    static func extraStatus(_ model: AppModel) -> String {
        #if RELIABILITY_STATUS
            if let status = model.projectsStatus { return " | status bar: \"\(status)\"" }
        #endif
        return ""
    }

    static func extraDashboard(_ model: AppModel) -> String {
        #if RELIABILITY_STATUS
            let status = model.dashboardFreshness.status(now: Date(), every: AppModel.dashboardEvery)
            if let status { return " | status bar: \"\(status)\"" }
        #endif
        return ""
    }

    static func run(_ name: String, _ body: () async throws -> String) async {
        let started = Date()
        do {
            let outcome = try await body()
            let log = try await gatewayLog()
            let elapsed = String(format: "%.1f", Date().timeIntervalSince(started))
            print("\(name) (\(elapsed)s)\n  app: \(outcome)\n  gateway: \(summary(log))")
        } catch {
            print("\(name): harness error \(error)")
        }
    }

    /// A model that has signed in and loaded once. The first read is a single
    /// request, so even an app that races its sign-ins starts warm; the
    /// gateway log is cleared after.
    static func warmModel(_ faults: [String: Any] = [:]) async throws -> AppModel {
        try await Self.faults(["reset": true].merging(faults) { $1 })
        let model = makeModel()
        await model.refreshDashboard()
        await model.refreshProjects()
        guard model.projectsError == nil, model.dashboardError == nil else {
            throw URLError(.cannotLoadFromNetwork)
        }
        try await Self.faults(["clear_log": true])
        return model
    }

    static func main() async {
        let arguments = Array(CommandLine.arguments.dropFirst())
        if arguments.first == "soak" {
            guard arguments.count == 3, let host = normalizeHost(arguments[1]), let minutes = Double(arguments[2])
            else {
                print("usage: soak <host> <minutes>")
                return
            }
            await soak(host: host, minutes: minutes)
            return
        }
        let only = Set(arguments)
        func wanted(_ name: String) -> Bool { only.isEmpty || only.contains(name) }

        if wanted("cold-fanout") {
            await run("cold-fanout: first open, 6 projects read in parallel") {
                try await faults(["reset": true, "projects": 6, "waiting": 1])
                let model = makeModel()
                await model.refreshProjects()
                return projectsView(model)
            }
        }
        if wanted("session-lost") {
            await run("session-lost: gateway restarted (deploy) or 30 min idle, then refresh") {
                let model = try await warmModel(["projects": 6, "waiting": 1])
                try await faults(["wipe": true])
                await model.refreshProjects()
                return projectsView(model)
            }
        }
        if wanted("waiting-burst") {
            await run("waiting-burst: Waiting tab with 9 waiting sessions") {
                let model = try await warmModel(["projects": 3, "waiting": 3])
                let client = model.client!
                await model.refreshWaiting()
                // Each waiting row's answer section reads its targets as it appears.
                let targets = await withTaskGroup(of: String?.self) { group in
                    for item in model.waiting {
                        group.addTask {
                            let outcome = await client.decisionTargets(
                                project: item.project.id, session: item.session.id)
                            if case .failed(let error) = outcome { return error.message }
                            return nil
                        }
                    }
                    var failures: [String] = []
                    for await failure in group { if let failure { failures.append(failure) } }
                    return failures
                }
                let targetText =
                    targets.isEmpty
                    ? "answer forms ok" : "\(targets.count) answer forms failed \(Set(targets).sorted())"
                return waitingView(model) + "; " + targetText
            }
        }
        if wanted("daemon-busy") {
            await run("daemon-busy: rsid answers busy twice (Store lock contention)") {
                let model = try await warmModel()
                try await faults(["busy_next": 2])
                await model.refreshProjects()
                return projectsView(model)
            }
        }
        if wanted("daemon-busy-cold") {
            await run("daemon-busy-cold: first open while rsid answers busy twice") {
                try await faults(["reset": true, "busy_next": 2])
                let model = makeModel()
                await model.refreshDashboard()
                return dashboardView(model)
            }
        }
        if wanted("daemon-restart") {
            await run("daemon-restart: rsid down for 6 s (a deploy restart)") {
                let model = try await warmModel()
                try await faults(["down_for_s": 6.0])
                await model.refreshProjects()
                let first = projectsView(model)
                await model.refreshDashboard()
                return first + "; dashboard: " + dashboardView(model)
            }
        }
        if wanted("dropped") {
            await run("dropped: the connection drops once (Tailscale path change)") {
                let model = try await warmModel()
                try await faults(["drop_next": 1])
                await model.refreshDashboard()
                return "dashboard: " + dashboardView(model)
            }
        }
        if wanted("localapi-403") {
            await run("localapi-403: one bare 403 from the identity check (localapi_busy)") {
                let model = try await warmModel()
                try await faults(["forbid_next": 1])
                await model.refreshDashboard()
                return "dashboard: " + dashboardView(model)
            }
        }
        if wanted("cancelled") {
            await run("cancelled: the screen goes away mid-refresh (tab switch)") {
                let model = try await warmModel(["read_ms": 400])
                let task = Task { await model.refreshProjects() }
                try await Task.sleep(for: .milliseconds(150))
                task.cancel()
                await task.value
                return projectsView(model)
            }
        }
        if wanted("outbox-offline") {
            await run("outbox-offline: a message is sent while the link drops and a reply is lost (#505)") {
                let model = try await warmModel(["projects": 1])
                let row = try Self.requireManager(model)
                let chat = try Self.requireChat(model, row)
                // Two sends dropped before the gateway sees them, one queued whose reply is lost,
                // then the link is fine: one message must reach the daemon, exactly once.
                try await faults(["drop_next": 2, "lose_reply_next": 1])
                chat.draft.text = "ship the migration, tell nobody"
                let sent = try chat.send().unwrap("a message was saved")
                let saved = chat.outgoing.first?.state.rawValue ?? "?"
                let started = Date()
                var tries = 0
                while Date().timeIntervalSince(started) < 90 {
                    try await Task.sleep(for: .milliseconds(250))
                    tries = max(tries, (model.outbox.message(sent.id)?.failures ?? 0) + 1)
                    if model.outbox.message(sent.id)?.state == .delivered { break }
                }
                let done = model.outbox.message(sent.id)
                let held = try await messages()
                return "message saved as \(saved) before any send; after \(String(format: "%.1f", Date().timeIntervalSince(started))) s state=\(done?.state.rawValue ?? "gone") tries=\(tries) failures=\(done?.failures ?? -1) deduplicated=\(done?.deduplicated ?? false)"
                    + "; daemon holds \(held.rows) message(s) after \(held.posts) POST(s), key reused: \(held.rows == 1 && held.keys == [sent.id])"
            }
        }
        if wanted("outbox-busy") {
            await run("outbox-busy: the daemon answers busy twice, three messages queued back to back (#505)") {
                let model = try await warmModel(["projects": 1])
                let row = try Self.requireManager(model)
                let chat = try Self.requireChat(model, row)
                try await faults(["message_busy_next": 2])
                var ids: [String] = []
                for text in ["first of three", "second of three", "third of three"] {
                    chat.draft.text = text
                    ids.append(try chat.send().unwrap("saved").id)
                }
                let started = Date()
                while Date().timeIntervalSince(started) < 90,
                    !ids.allSatisfy({ model.outbox.message($0)?.state == .delivered })
                {
                    try await Task.sleep(for: .milliseconds(250))
                }
                let held = try await messages()
                let states = ids.map { model.outbox.message($0)?.state.rawValue ?? "gone" }
                return "states \(states) after \(String(format: "%.1f", Date().timeIntervalSince(started))) s; daemon holds \(held.rows) message(s) after \(held.posts) POST(s), in order: \(held.texts == ["first of three", "second of three", "third of three"])"
            }
        }
        if wanted("stall") {
            await run("stall: one read hangs past the 15 s request timeout") {
                let model = try await warmModel()
                try await faults(["stall_next": 1, "stall_s": 20.0])
                await model.refreshDashboard()
                return "dashboard: " + dashboardView(model)
            }
        }
    }
}

@main
struct HarnessMain {
    static func main() async {
        await Harness.main()
    }
}

/// A failure in a few words: the fixed app's headline, else its message.
func describe(_ error: RemoteError) -> String {
    #if RELIABILITY_STATUS
        error.headline
    #else
        error.message
    #endif
}

/// Projects whose session list the gateway cut at its first page (fixed app only).
@MainActor
func partialCount(_ model: AppModel) -> Int {
    #if RELIABILITY_STATUS
        model.projects.filter(\.partial).count
    #else
        0
    #endif
}

// MARK: Messages (#505)

struct HeldMessages {
    var rows: Int
    var posts: Int
    var keys: [String]
    var texts: [String]
}

extension Optional {
    func unwrap(_ what: String) throws -> Wrapped {
        guard let self else { throw HarnessError(what: "missing: \(what)") }
        return self
    }
}

struct HarnessError: Error { let what: String }

extension Harness {
    /// The first project with a manager seat, after the model's warm-up read.
    static func requireManager(_ model: AppModel) throws -> ProjectRow {
        try model.projects.first { $0.seats?.projectManager != nil }.unwrap("a project with a manager seat")
    }

    static func requireChat(_ model: AppModel, _ row: ProjectRow) throws -> ManagerChat {
        try model.chat(project: row.project.id).unwrap("a chat")
    }

    /// What the fake daemon queued.
    static func messages() async throws -> HeldMessages {
        struct Body: Decodable {
            struct Row: Decodable {
                let key: String
                let text: String
            }
            let messages: [Row]
            let posts: Int
        }
        let (data, _) = try await control.data(from: URL(string: "https://\(host)/__messages")!)
        let body = try JSONDecoder().decode(Body.self, from: data)
        return HeldMessages(
            rows: body.messages.count, posts: body.posts, keys: body.messages.map(\.key), texts: body.messages.map(\.text))
    }
}
