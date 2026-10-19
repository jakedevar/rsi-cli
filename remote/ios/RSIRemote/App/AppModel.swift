import Foundation
import Observation

/// A project row: the exposed project plus what its sessions say.
struct ProjectRow: Identifiable, Sendable, Hashable {
    let project: WireProject
    /// Sessions waiting on the operator; nil until the session list loads.
    var waiting: Int?
    var sessions: [SessionSummary]
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
/// `RemoteClient` actor.
@MainActor
@Observable
final class AppModel {
    static let hostKey = "gatewayHost"

    private(set) var host: String?
    private(set) var client: RemoteClient?

    var projects: [ProjectRow] = []
    var projectsError: RemoteError?
    var loadingProjects = false
    /// The last `GET /api/v2/projects` failure (an older gateway answers 404).
    var seatsError: RemoteError?
    /// The gateway's last `messages_enabled` (hub #2988); nil if it never said.
    private(set) var messagesEnabled: Bool?

    var waiting: [WaitingItem] = []
    var waitingError: RemoteError?
    var loadingWaiting = false
    /// Bumped after each Waiting refresh so the answer forms read again.
    var waitingGeneration = 0

    /// Answer drafts for every session; they outlive a refresh.
    let drafts = AnswerDrafts()

    /// One manager chat per project; its drafts and loaded pages outlive navigation.
    @ObservationIgnored private var chats: [String: ManagerChat] = [:]

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        if let saved = defaults.string(forKey: Self.hostKey), let host = normalizeHost(saved) {
            self.host = host
            client = RemoteClient(host: host)
        }
    }

    /// Save the host the operator typed. Returns false when it is not a host.
    @discardableResult
    func setHost(_ raw: String) -> Bool {
        guard let host = normalizeHost(raw) else { return false }
        if host != self.host {
            self.host = host
            client = RemoteClient(host: host)
            defaults.set(host, forKey: Self.hostKey)
            projects = []
            waiting = []
            chats = [:]
        }
        return true
    }

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

    /// Projects, then each project's first session page (for the badge).
    func refreshProjects() async {
        loadingProjects = true
        defer { loadingProjects = false }
        do {
            let client = try requireClient()
            async let seatsRead = Self.readSeats(client)
            let list = try await client.projects()
            var rows = list.items.map { ProjectRow(project: $0, waiting: nil, sessions: []) }
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
                        rows[index].waiting = page.items.filter(\.needsYou).count
                    case .failure(let error):
                        rows[index].error = error.message
                    }
                }
            }
            apply(await seatsRead, to: &rows)
            projects = rows
            projectsError = nil
        } catch let error as RemoteError {
            projectsError = error
        } catch {
            projectsError = .invalidResponse("projects")
        }
    }

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
            for (project, chat) in chats {
                chat.update(seats: byId[project])
                chat.update(messagesEnabled: list.messagesEnabled)
            }
        case .failure(let error):
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
            onSeats: { [weak self] list in self?.applySeats(list) })
        chat.update(messagesEnabled: messagesEnabled)
        chats[project] = chat
        return chat
    }

    /// The current seats of one project, as last listed.
    func seats(project: String) -> ProjectSeats? {
        projects.first { $0.project.id == project }?.seats
    }

    /// Every session waiting on the operator, with its decisions.
    func refreshWaiting() async {
        loadingWaiting = true
        defer { loadingWaiting = false }
        if projects.isEmpty { await refreshProjects() }
        guard let client else {
            waitingError = .notConfigured
            return
        }
        var items: [WaitingItem] = projects.flatMap { row in
            row.sessions.filter(\.needsYou).map {
                WaitingItem(project: row.project, session: $0, decisions: [])
            }
        }
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
                case .success(let list): items[index].decisions = list.items.filter(\.isOpen)
                case .failure(let error): items[index].error = error.message
                }
            }
        }
        waiting = items
        waitingError = projectsError
        waitingGeneration += 1
    }

    var waitingCount: Int { projects.reduce(0) { $0 + ($1.waiting ?? 0) } }
}
