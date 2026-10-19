import Foundation
import Observation

/// One seat's transcript as loaded so far, newest first.
struct SeatThread: Sendable, Equatable {
    /// The seat session these items belong to; a rotated seat starts a new thread.
    let seat: String
    var items: [ConversationItem] = []
    /// `next_before` of the oldest page loaded; nil when the oldest page is in.
    var olderCursor: String?
    var hasMore = false
    /// Delivery view of the newest operator messages (first page only).
    var operatorMessages: [OperatorMessageState] = []
    /// The seat as the last page described it (status, model, activity).
    var seatInfo: ManagerSeat?
    var loaded = false
    var error: RemoteError?
    var loadingOlder = false
    var olderError: RemoteError?

    /// Merge a freshly polled first page. Overlap keeps the older pages already
    /// loaded; a page with no overlap (more than one page arrived between polls)
    /// replaces the thread so it never shows a silent gap.
    mutating func mergeFirstPage(_ page: ConversationPage) {
        defer {
            operatorMessages = page.operatorMessages
            seatInfo = page.seat
            loaded = true
            error = nil
        }
        let known = Set(items.map(\.id))
        let gap: Bool = {
            guard loaded, page.hasMore, let oldestNew = page.items.last, let newestKnown = items.first
            else { return false }
            return !known.contains(oldestNew.id) && ConversationItem.newerFirst(oldestNew, newestKnown)
        }()
        if !loaded || gap || items.isEmpty {
            items = page.items
            olderCursor = page.nextBefore
            hasMore = page.hasMore
            return
        }
        let fresh = Set(page.items.map(\.id))
        items = (page.items + items.filter { !fresh.contains($0.id) }).sorted(by: ConversationItem.newerFirst)
    }

    /// Append an older page fetched with `olderCursor`.
    mutating func appendOlderPage(_ page: ConversationPage) {
        let known = Set(items.map(\.id))
        items = (items + page.items.filter { !known.contains($0.id) }).sorted(by: ConversationItem.newerFirst)
        olderCursor = page.nextBefore
        hasMore = page.hasMore
        olderError = nil
    }
}

/// The manager chat of one project: a PM / Global seat toggle, each seat's
/// conversation (polled while visible) and its compose box.
@MainActor
@Observable
final class ManagerChat {
    let project: String
    private let client: RemoteClient
    private let onSeats: @MainActor (ProjectSeatsList) -> Void
    private let newKey: () -> String

    private(set) var seats: ProjectSeats?
    var role: SeatRole = .projectManager
    private(set) var threads: [SeatRole: SeatThread] = [:]
    var drafts: [SeatRole: MessageDraft] = [:]
    /// Messaging is off: the gateway said so (`messages_enabled: false`, hub
    /// #2988) or refused a send while the seat is listed (an older gateway).
    private(set) var messagingOff = false
    /// The gateway's `messages_enabled`, nil until a seat list carries it.
    private(set) var messagesEnabled: Bool?

    /// The compose box is closed: the gateway reports messaging off.
    var composeDisabled: Bool { messagesEnabled == false }
    private(set) var backoff = PollBackoff()
    var refreshing = false

    init(
        project: String, seats: ProjectSeats?, client: RemoteClient,
        onSeats: @escaping @MainActor (ProjectSeatsList) -> Void = { _ in },
        newKey: @escaping () -> String = { UUID().uuidString.lowercased() }
    ) {
        self.project = project
        self.seats = seats
        self.client = client
        self.onSeats = onSeats
        self.newKey = newKey
    }

    var seat: ManagerSeat? { seats?.seat(role) }

    /// The selected seat's thread, if it belongs to the current seat session.
    var thread: SeatThread? {
        guard let seat, let thread = threads[role], thread.seat == seat.sessionId else { return nil }
        return thread
    }

    var draft: MessageDraft {
        get { drafts[role] ?? MessageDraft() }
        set { drafts[role] = newValue }
    }

    /// Take a newer seat list from elsewhere (the Projects refresh).
    func update(seats: ProjectSeats?) {
        if let seats { self.seats = seats }
    }

    /// Take the gateway's `messages_enabled` (nil: an older gateway, keep what we know).
    func update(messagesEnabled: Bool?) {
        guard let messagesEnabled else { return }
        self.messagesEnabled = messagesEnabled
        messagingOff = !messagesEnabled
    }

    // MARK: Reading

    /// Read the selected seat's first page and merge it. Updates the backoff.
    func refresh() async {
        guard let seat else { return }
        let role = self.role
        refreshing = true
        defer { refreshing = false }
        do throws(ReadFailure) {
            let page = try await client.conversation(project: project, seat: seat.sessionId)
            var thread = threads[role].flatMap { $0.seat == seat.sessionId ? $0 : nil }
                ?? SeatThread(seat: seat.sessionId)
            thread.mergeFirstPage(page)
            threads[role] = thread
            backoff.succeeded()
        } catch {
            backoff.failed(retryAfter: error.retryAfter)
            var thread = threads[role].flatMap { $0.seat == seat.sessionId ? $0 : nil }
                ?? SeatThread(seat: seat.sessionId)
            thread.error = error.error
            threads[role] = thread
            // A stale seat id reads as 404 (or 403): re-list, then read the new seat.
            if case .server(404, _) = error.error, await relist(seat: seat.sessionId) == false,
                self.seat?.sessionId != seat.sessionId
            {
                await refresh()
            } else if case .forbidden = error.error {
                _ = await relist(seat: seat.sessionId)
            }
        }
    }

    func loadOlder() async {
        guard let seat, let start = thread, let cursor = start.olderCursor, !start.loadingOlder
        else { return }
        let role = self.role
        threads[role]?.loadingOlder = true
        var page: ConversationPage?
        var failure: RemoteError?
        do throws(ReadFailure) {
            page = try await client.conversation(project: project, seat: seat.sessionId, before: cursor)
        } catch {
            failure = error.error
        }
        // A poll may have replaced the thread meanwhile (seat rotation or a gap reset).
        guard var current = threads[role], current.seat == start.seat else { return }
        current.loadingOlder = false
        if let page, current.olderCursor == cursor {
            current.appendOlderPage(page)
        } else if let failure {
            current.olderError = failure
        }
        threads[role] = current
    }

    /// Poll while the view is visible: `.task` cancels it on disappear.
    func poll() async {
        while !Task.isCancelled {
            await refresh()
            do {
                try await Task.sleep(for: backoff.interval)
            } catch {
                return
            }
        }
    }

    /// Re-list the seats. Returns whether `seat` is still one of this
    /// project's seats, or nil when the list could not be read.
    @discardableResult
    func relist(seat: String) async -> Bool? {
        do {
            let list = try await client.managerSeats()
            onSeats(list)
            update(messagesEnabled: list.messagesEnabled)
            let mine = list.projects.first { $0.projectId == project }
            if let mine { seats = mine }
            return mine?.lists(seat) ?? false
        } catch {
            return nil
        }
    }

    // MARK: Sending

    /// Send (or retry the frozen attempt of) the selected seat's draft.
    func send() async {
        let role = self.role
        guard let seat else { return }
        var draft = drafts[role] ?? MessageDraft()
        let request: MessageRequest
        do {
            request = try draft.begin(project: project, seat: seat.sessionId, newKey: newKey)
        } catch {
            draft.finish(.failure(error as? MessageError ?? MessageError(.invalid, "\(error)")))
            drafts[role] = draft
            return
        }
        drafts[role] = draft
        let result: Result<MessageReceipt, MessageError>
        do throws(MessageError) {
            result = .success(try await client.sendMessage(request))
        } catch {
            result = .failure(await refine(error, seat: request.seat))
        }
        var finished = drafts[role] ?? draft
        finished.finish(result)
        drafts[role] = finished
        switch result {
        case .success:
            // A send that queued proves messaging is on.
            messagingOff = false
            if messagesEnabled == false { messagesEnabled = true }
            await refresh()
        case .failure(let error) where error.kind == .messagingOff:
            messagingOff = true
        case .failure:
            break
        }
    }

    /// Give up on an unknown attempt.
    func discard() {
        var draft = self.draft
        draft.discard()
        self.draft = draft
    }

    /// A bare 403 names no reason. Re-list the seats: a seat that is still
    /// listed leaves the `allow_messages` switch as the reason (this client
    /// always sends the exact Origin and its session's CSRF); a seat that is
    /// gone is a rotation. A 404 or `seat_not_live` also re-lists.
    private func refine(_ error: MessageError, seat: String) async -> MessageError {
        switch error.kind {
        case .forbidden:
            switch await relist(seat: seat) {
            // A gateway that reports messaging on refused for another reason.
            case true? where messagesEnabled == true: return error
            case true?: return .messagingOff()
            case false?: return .seatChanged(status: error.status)
            case nil: return error
            }
        case .seatChanged:
            await relist(seat: seat)
            return error
        default:
            return error
        }
    }

    /// The delivery line for a send receipt: its state as the conversation's
    /// `operator_messages` reports it now, else the receipt's own.
    func delivery(of receipt: MessageReceipt) -> (text: String, tone: AnswerTone) {
        let state = thread?.operatorMessages.first { $0.messageId == receipt.messageId }?.state
            ?? receipt.state
        let (text, tone) = describeDelivery(state)
        let replay = receipt.deduplicated ? "Already queued earlier under this key; not sent twice. " : ""
        return (replay + text, tone)
    }
}
