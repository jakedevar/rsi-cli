import Foundation
import Observation

/// A project row: the exposed project plus what its sessions say.
struct ProjectRow: Identifiable, Sendable, Hashable {
    let project: WireProject
    /// Sessions waiting on the operator; nil until the session list loads.
    var waiting: Int?
    var sessions: [SessionSummary]
    /// The hub has more sessions than its first page: the gateway lists them in
    /// id order, so the counts and the waiting ones here are from that page only.
    var partial = false
    var error: String?
    /// The v2 manager seats; nil until `GET /api/v2/projects` lists the project.
    var seats: ProjectSeats?

    var id: String { project.id }

    /// The row badge: the PM ledger's pending decisions where v2 gives them,
    /// else the v1 count of sessions waiting on the operator.
    var badge: String? {
        if let seats { return seats.projectManager?.pendingBadge }
        guard let waiting, waiting > 0 else { return nil }
        return "\(waiting)"
    }
}

/// One session that waits on the operator, with its decisions once read.
struct WaitingItem: Identifiable, Sendable, Hashable {
    let project: WireProject
    let session: SessionSummary
    var decisions: [Decision]
    var error: String?

    var id: String { session.id }
}

/// The app's state. Views read it on the main actor; reads go through the
/// `RemoteClient` actor. Each screen keeps its last good data with its age
/// (`Freshness`) when a refresh fails, and reads again by itself while shown
/// (#350).
@MainActor
@Observable
final class AppModel {
    static let hostKey = "gatewayHost"
    /// How often a shown screen reads again by itself while all is well.
    static let projectsEvery: Duration = .seconds(60)
    static let waitingEvery: Duration = .seconds(60)
    static let dashboardEvery: Duration = .seconds(30)
    /// A screen that reappears within this many seconds of a good read shows
    /// that read instead of asking again (tab switches, navigation back).
    static let reuseFor: TimeInterval = 15

    private(set) var host: String?
    private(set) var client: RemoteClient?

    var projects: [ProjectRow] = []
    /// When the projects were last read, and why the latest refresh failed.
    private(set) var projectsFreshness = Freshness()
    var projectsError: RemoteError? { projectsFreshness.error }
    var loadingProjects = false
    /// The last `GET /api/v2/projects` failure (an older gateway answers 404).
    var seatsError: RemoteError?
    /// The gateway's last `messages_enabled` (hub #2988); nil if it never said.
    private(set) var messagesEnabled: Bool?

    /// The last `GET /api/v2/dashboard` (hub #3287); kept while a refresh fails.
    var dashboard: Dashboard?
    private(set) var dashboardFreshness = Freshness()
    var dashboardError: RemoteError? { dashboardFreshness.error }
    var loadingDashboard = false

    var waiting: [WaitingItem] = []
    private(set) var waitingFreshness = Freshness()
    var waitingError: RemoteError? { waitingFreshness.error }
    var loadingWaiting = false
    /// Bumped after each Waiting refresh so the answer forms read again.
    var waitingGeneration = 0

    /// Bumped each time the app comes to the foreground, so the screen on
    /// show reads again when its data is stale.
    private(set) var foregroundTick = 0

    /// Answer drafts for every session; they outlive a refresh.
    let drafts = AnswerDrafts()

    /// The Provider usage screen's data (#504); it outlives navigation.
    let providerUsageSource: ProviderUsageSource
    let providerUsage: ProviderUsageStore

    /// One manager chat per project; its drafts and loaded pages outlive navigation.
    @ObservationIgnored private var chats: [String: ManagerChat] = [:]

    private let defaults: UserDefaults
    /// Builds the gateway client for a host. Tests and the reliability
    /// harness (remote/ios/tools/reliability) inject one with their own session.
    private let makeClient: @Sendable (String) -> RemoteClient
    /// The clock; tests move it.
    @ObservationIgnored var now: @MainActor () -> Date = { Date() }

    /// Messages written in the app wait here until the hub has them (#505).
    let outbox: Outbox
    /// A launch-argument fault: sends go to an unreachable host until then,
    /// the saved host untouched (device verification, `OutboxLaunch`).
    @ObservationIgnored private var sendFault: (client: RemoteClient, until: Date)?

    init(
        defaults: UserDefaults = .standard,
        outbox: Outbox = Outbox(store: MemoryOutboxStore()),
        makeClient: @escaping @Sendable (String) -> RemoteClient = { RemoteClient(host: $0) }
    ) {
        self.defaults = defaults
        self.makeClient = makeClient
        self.outbox = outbox
        providerUsageSource = ProviderUsageSource.current(defaults)
        providerUsage = ProviderUsageStore(source: providerUsageSource)
        if let saved = defaults.string(forKey: Self.hostKey), let host = normalizeHost(saved) {
            self.host = host
            client = makeClient(host)
        }
        providerUsage.client = { [weak self] in self?.client }
        outbox.transport = self
        outbox.onDelivered = { [weak self] message in
            self?.chats[message.project]?.accepted()
        }
    }

    /// Save the host the operator typed. Returns false when it is not a host.
    @discardableResult
    func setHost(_ raw: String) -> Bool {
        guard let host = normalizeHost(raw) else { return false }
        if host != self.host {
            self.host = host
            client = makeClient(host)
            defaults.set(host, forKey: Self.hostKey)
            projects = []
            projectsFreshness = Freshness()
            waiting = []
            waitingFreshness = Freshness()
            dashboard = nil
            dashboardFreshness = Freshness()
            providerUsage.reset()
            chats = [:]
        }
        outbox.kick()
        return true
    }

    /// The app became active again.
    func foregrounded() {
        foregroundTick += 1
        outbox.kick()
    }

    /// The identity of the shown screens' refresh loops: a new host or a
    /// return to the foreground restarts them.
    var refreshKey: String { "\(host ?? "")#\(foregroundTick)" }

    private func requireClient() throws -> RemoteClient {
        guard let client else { throw RemoteError.notConfigured }
        return client
    }

    /// Settings → Test connection: the gateway's `RemoteGetInfoV1`.
    func testConnection() async -> Result<InfoResult, RemoteError> {
        do {
            return .success(try await requireClient().info())
        } catch let error as RemoteError {
            return .failure(error)
        } catch {
            return .failure(.invalidResponse("\(error)"))
        }
    }

    // MARK: Projects

    /// Projects, then each project's first session page (for the badge). A
    /// failed read keeps what was shown (a project whose sessions could not be
    /// read keeps its last ones and says so); a cancelled refresh changes nothing.
    func refreshProjects() async {
        guard let client else {
            projectsFreshness.failed(.notConfigured, at: now())
            return
        }
        loadingProjects = true
        defer { loadingProjects = false }
        async let seatsRead = Self.readSeats(client)
        let list: ProjectListResult
        do {
            list = try await client.projects()
        } catch {
            projectsFreshness.failed(error as? RemoteError ?? .invalidResponse("projects"), at: now())
            return
        }
        let previous = Dictionary(projects.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        var rows = list.items.map { project in
            var row = ProjectRow(project: project, waiting: nil, sessions: [])
            if let old = previous[project.id] {
                row.waiting = old.waiting
                row.sessions = old.sessions
                row.partial = old.partial
                row.seats = old.seats
            }
            return row
        }
        var cancelled = false
        await withTaskGroup(of: (Int, Result<SessionListResult, RemoteError>).self) { group in
            for (index, row) in rows.enumerated() {
                group.addTask {
                    do {
                        return (index, .success(try await client.sessions(project: row.project.id)))
                    } catch let error as RemoteError {
                        return (index, .failure(error))
                    } catch {
                        return (index, .failure(.invalidResponse("sessions")))
                    }
                }
            }
            for await (index, result) in group {
                switch result {
                case .success(let page):
                    rows[index].sessions = page.items
                    rows[index].partial = page.nextCursor != nil
                    rows[index].waiting = page.items.filter(\.needsYou).count
                case .failure(.cancelled):
                    cancelled = true
                case .failure(let error):
                    rows[index].error =
                        previous[rows[index].id] == nil
                        ? error.message : "\(error.headline) · sessions not refreshed"
                }
            }
        }
        let seats = await seatsRead
        if cancelled || Task.isCancelled { return }
        apply(seats, to: &rows)
        projects = rows
        projectsFreshness.succeeded(at: now())
    }

    /// Read the projects unless the last good read is younger than `maxAge` seconds.
    func refreshProjects(ifOlderThan maxAge: TimeInterval) async {
        guard projectsFreshness.isStale(maxAge, now: now()) else { return }
        await refreshProjects()
    }

    /// While the Projects screen is shown: read now unless fresh, then every
    /// minute, or on the backoff after a failure. Ends when cancelled.
    func keepProjectsFresh() async {
        while client != nil, !Task.isCancelled {
            await refreshProjects(ifOlderThan: Self.reuseFor)
            do {
                try await Task.sleep(for: projectsFreshness.wait(every: Self.projectsEvery, now: now()))
            } catch {
                return
            }
        }
    }

    /// The status line over the project list; nil while fresh and fine.
    var projectsStatus: String? { projectsFreshness.status(now: now(), every: Self.projectsEvery) }

    private nonisolated static func readSeats(_ client: RemoteClient) async -> Result<ProjectSeatsList, RemoteError> {
        do {
            return .success(try await client.managerSeats())
        } catch let error as RemoteError {
            return .failure(error)
        } catch {
            return .failure(.invalidResponse("manager seats"))
        }
    }

    private func apply(_ result: Result<ProjectSeatsList, RemoteError>, to rows: inout [ProjectRow]) {
        switch result {
        case .success(let list):
            seatsError = nil
            let byId = Dictionary(list.projects.map { ($0.projectId, $0) }, uniquingKeysWith: { a, _ in a })
            for index in rows.indices { rows[index].seats = byId[rows[index].project.id] }
            messagesEnabled = list.messagesEnabled ?? messagesEnabled
            if list.messagesEnabled == true { outbox.messagingEnabled() }
            for (project, chat) in chats {
                chat.update(seats: byId[project])
                chat.update(messagesEnabled: list.messagesEnabled)
            }
        case .failure(.cancelled):
            break
        case .failure(let error):
            // The rows keep the seats read before.
            seatsError = error
        }
    }

    /// A newer seat list (the chat re-lists after a refusal; a seat session can rotate).
    func applySeats(_ list: ProjectSeatsList) {
        apply(.success(list), to: &projects)
    }

    /// The manager chat of `project`, created on first use.
    func chat(project: String) -> ManagerChat? {
        if let chat = chats[project] { return chat }
        guard let client else { return nil }
        let chat = ManagerChat(
            project: project, seats: seats(project: project), client: client,
            host: host ?? "", outbox: outbox,
            onSeats: { [weak self] list in self?.applySeats(list) })
        chat.update(messagesEnabled: messagesEnabled)
        chats[project] = chat
        return chat
    }

    /// The current seats of one project, as last listed.
    func seats(project: String) -> ProjectSeats? {
        projects.first { $0.project.id == project }?.seats
    }

    /// The project row as last read (a pushed screen shows the live one).
    func project(_ id: String) -> ProjectRow? {
        projects.first { $0.id == id }
    }

    // MARK: Dashboard

    /// The dashboard: satellites and running agents per provider and effort.
    func refreshDashboard() async {
        guard let client else {
            dashboardFreshness.failed(.notConfigured, at: now())
            return
        }
        loadingDashboard = true
        defer { loadingDashboard = false }
        do {
            dashboard = try await client.dashboard()
            dashboardFreshness.succeeded(at: now())
        } catch {
            dashboardFreshness.failed(error as? RemoteError ?? .invalidResponse("dashboard"), at: now())
        }
    }

    /// While the Dashboard is shown: read now unless fresh, then every 30 s,
    /// or on the backoff after a failure. Ends when cancelled.
    func keepDashboardFresh() async {
        while client != nil, !Task.isCancelled {
            if dashboardFreshness.isStale(Self.reuseFor, now: now()) { await refreshDashboard() }
            do {
                try await Task.sleep(for: dashboardFreshness.wait(every: Self.dashboardEvery, now: now()))
            } catch {
                return
            }
        }
    }

    // MARK: Waiting

    /// Every session waiting on the operator, with its decisions. The session
    /// lists are read again first unless they are only seconds old, so a new
    /// waiting session shows up here without a visit to Projects.
    func refreshWaiting() async {
        guard let client else {
            waitingFreshness.failed(.notConfigured, at: now())
            return
        }
        loadingWaiting = true
        defer { loadingWaiting = false }
        await refreshProjects(ifOlderThan: 10)
        if Task.isCancelled { return }
        if projectsFreshness.updatedAt == nil, let error = projectsFreshness.error {
            waitingFreshness.failed(error, at: now())
            return
        }
        let previous = Dictionary(waiting.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        var items: [WaitingItem] = projects.flatMap { row in
            row.sessions.filter(\.needsYou).map {
                WaitingItem(project: row.project, session: $0, decisions: previous[$0.id]?.decisions ?? [])
            }
        }
        var cancelled = false
        await withTaskGroup(of: (Int, Result<DecisionListResult, RemoteError>).self) { group in
            for (index, item) in items.enumerated() {
                group.addTask {
                    do {
                        return (index, .success(try await client.decisions(
                            project: item.project.id, session: item.session.id)))
                    } catch let error as RemoteError {
                        return (index, .failure(error))
                    } catch {
                        return (index, .failure(.invalidResponse("decisions")))
                    }
                }
            }
            for await (index, result) in group {
                switch result {
                case .success(let list):
                    items[index].decisions = list.items.filter(\.isOpen)
                case .failure(.cancelled):
                    cancelled = true
                case .failure(let error):
                    items[index].error =
                        items[index].decisions.isEmpty
                        ? error.message : "\(error.headline) · decisions not refreshed"
                }
            }
        }
        if cancelled || Task.isCancelled { return }
        waiting = items
        if let error = projectsFreshness.error {
            waitingFreshness.failed(error, at: now())
        } else {
            waitingFreshness.succeeded(at: now())
        }
        waitingGeneration += 1
    }

    /// While the Waiting screen is shown: read now unless fresh, then every
    /// minute, or on the backoff after a failure. Ends when cancelled.
    func keepWaitingFresh() async {
        while client != nil, !Task.isCancelled {
            if waitingFreshness.isStale(Self.reuseFor, now: now()) { await refreshWaiting() }
            do {
                try await Task.sleep(for: waitingFreshness.wait(every: Self.waitingEvery, now: now()))
            } catch {
                return
            }
        }
    }

    var waitingCount: Int { projects.reduce(0) { $0 + ($1.waiting ?? 0) } }

    /// Projects whose session list was only partly read, so Waiting may miss some.
    var partlyCheckedProjects: [String] { projects.filter(\.partial).map(\.project.name) }
}

// MARK: Outbox transport

extension AppModel: OutboxTransport {
    var activeHost: String? { host }

    func currentSeat(project: String, role: String) -> String? {
        guard let role = SeatRole(rawValue: role) else { return nil }
        return seats(project: project)?.seat(role)?.sessionId
    }

    /// Take the outbox's sends to an unreachable host until `until`: a real
    /// network failure at the transport, the saved host unchanged (the device
    /// stand-in for airplane mode, which cannot be switched from a Mac).
    func simulateUnreachableSends(until: Date) {
        sendFault = (makeClient("rsi-outbox-fault.invalid"), until)
    }

    private func sendingClient() -> RemoteClient? {
        if let fault = sendFault {
            if now() < fault.until { return fault.client }
            sendFault = nil
        }
        return client
    }

    func deliver(_ request: MessageRequest) async -> Result<MessageReceipt, MessageError> {
        guard let client = sendingClient() else {
            return .failure(
                MessageError(.unavailable, "No RSI host is set. Nothing was sent; it will send once a host is set."))
        }
        do throws(MessageError) {
            return .success(try await client.sendMessage(request))
        } catch {
            return .failure(await refine(error, project: request.project, seat: request.seat))
        }
    }

    /// A bare 403 names no reason. Re-list the seats: a seat that is still
    /// listed leaves the `allow_messages` switch as the reason (this client
    /// always sends the exact Origin and its session's CSRF); a seat that is
    /// gone is a rotation. A 404 or `seat_not_live` also re-lists.
    private func refine(_ error: MessageError, project: String, seat: String) async -> MessageError {
        switch error.kind {
        case .forbidden, .seatChanged:
            guard let client, let list = try? await client.managerSeats() else { return error }
            applySeats(list)
            guard error.kind == .forbidden else { return error }
            let listed = list.projects.first { $0.projectId == project }?.lists(seat) ?? false
            // A gateway that reports messaging on refused for another reason.
            if listed, messagesEnabled == true { return error }
            return listed ? .messagingOff() : .seatChanged(status: error.status)
        default:
            return error
        }
    }
}

// MARK: Launch arguments (device verification)

/// Launch arguments for checking the outbox on a phone that cannot be tapped
/// from the Mac (`devicectl device process launch … -outboxFaultSeconds 25
/// -outboxProbe "text"`), as `-initialTab` does for screens (#492). Neither
/// changes the saved host.
enum OutboxLaunch {
    /// Sends go to an unreachable host for this many seconds after launch.
    static let faultKey = "outboxFaultSeconds"
    /// A message to put in the outbox for the first project's manager.
    static let probeKey = "outboxProbe"
    /// Open the first project that has a manager seat on the Projects tab, once.
    static let openKey = "outboxOpenChat"

    static func faultSeconds(_ defaults: UserDefaults) -> TimeInterval? {
        guard let text = defaults.string(forKey: faultKey), let seconds = TimeInterval(text), seconds > 0
        else { return nil }
        return min(seconds, 600)
    }

    static func probeText(_ defaults: UserDefaults) -> String? {
        guard let text = defaults.string(forKey: probeKey), messageProblem(text) == nil else { return nil }
        return text
    }
}

extension AppModel {
    /// The project to open for `-outboxOpenChat`, once projects are listed.
    func launchChatProject() -> String? {
        guard defaults.string(forKey: OutboxLaunch.openKey) != nil else { return nil }
        return projects.first { $0.seats?.projectManager != nil }?.id
    }

    /// Start the outbox, then run the launch-argument checks.
    func start() async {
        if let seconds = OutboxLaunch.faultSeconds(defaults) {
            simulateUnreachableSends(until: now().addingTimeInterval(seconds))
        }
        outbox.kick()
        guard let text = OutboxLaunch.probeText(defaults), let host else { return }
        // A relaunch with the same probe checks what the outbox kept; it adds nothing.
        guard !outbox.entries.contains(where: { $0.host == host && $0.text == text }) else { return }
        if projects.isEmpty { await refreshProjects() }
        guard let row = projects.first(where: { $0.seats?.projectManager != nil }),
            let seat = row.seats?.projectManager
        else { return }
        _ = try? outbox.enqueue(
            host: host, project: row.project.id, role: SeatRole.projectManager.rawValue,
            seat: seat.sessionId, text: text)
    }
}
