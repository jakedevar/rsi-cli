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

    /// The hub's delivery state of the operator message `event` shows: by the
    /// hub id on the item, else by the key the phone sent (hub #3797).
    func deliveryState(of event: ChatEvent) -> String? {
        operatorMessages.first { record in
            if let id = event.operatorMessageId, record.messageId == id { return true }
            if let key = event.idempotencyKey, let echoed = record.idempotencyKey {
                return key.lowercased() == echoed.lowercased()
            }
            return false
        }?.state
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
    /// The gateway host this chat reads and writes for.
    let host: String
    /// Where sent messages wait until the hub has them (#505).
    let outbox: Outbox
    private let client: RemoteClient
    private let onSeats: @MainActor (ProjectSeatsList) -> Void

    private(set) var seats: ProjectSeats?
    var role: SeatRole = .projectManager
    private(set) var threads: [SeatRole: SeatThread] = [:]
    var drafts: [SeatRole: MessageDraft] = [:]
    /// Messaging is off: the gateway said so (`messages_enabled: false`, hub
    /// #2988) or refused a queued message while the seat is listed (an older
    /// gateway).
    private var flagOff = false
    var messagingOff: Bool { flagOff || heldForMessagingOff }
    /// The gateway's `messages_enabled`, nil until a seat list carries it.
    private(set) var messagesEnabled: Bool?

    /// The compose box is closed: the gateway reports messaging off.
    var composeDisabled: Bool { messagesEnabled == false }
    private(set) var backoff = PollBackoff()
    var refreshing = false

    init(
        project: String, seats: ProjectSeats?, client: RemoteClient,
        host: String = "", outbox: Outbox = Outbox(store: MemoryOutboxStore()),
        onSeats: @escaping @MainActor (ProjectSeatsList) -> Void = { _ in }
    ) {
        self.project = project
        self.seats = seats
        self.client = client
        self.onSeats = onSeats
        self.host = host
        self.outbox = outbox
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
        flagOff = !messagesEnabled
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
            // The pane went away mid-read: nothing failed.
            if error.error == .cancelled { return }
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
        } else if let failure, failure != .cancelled {
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

    /// Save the selected seat's draft in the outbox and start sending it. The
    /// box is empty again at once; the outbox keeps the message until the hub
    /// has it, however long that takes (#505). Nothing is saved for a text the
    /// gateway would refuse; the reason shows on the draft.
    @discardableResult
    func send() -> OutboxMessage? {
        guard let seat else { return nil }
        var draft = self.draft
        do {
            let message = try outbox.enqueue(
                host: host, project: project, role: role.rawValue, seat: seat.sessionId, text: draft.text)
            draft.text = ""
            draft.problem = nil
            self.draft = draft
            return message
        } catch {
            draft.problem = error.message
            self.draft = draft
            return nil
        }
    }

    /// This project's outbox messages on this host, newest first.
    var outgoing: [OutboxMessage] { outbox.messages(host: host, project: project) }

    /// Messages held because the hub has messaging switched off.
    private var heldForMessagingOff: Bool {
        outgoing.contains {
            $0.state == .queued && $0.lastErrorKind == MessageErrorKind.messagingOff.rawValue
        }
    }

    /// The hub accepted a message from this chat: messaging is on.
    func accepted() {
        flagOff = false
        if messagesEnabled == false { messagesEnabled = true }
        Task { await refresh() }
    }

    /// The delivery line for a message in the outbox, with its state as the
    /// conversation's `operator_messages` reports it now, else the hub's own
    /// at acceptance.
    func delivery(of message: OutboxMessage) -> (text: String, tone: AnswerTone)? {
        guard message.state == .delivered, let accepted = message.hubState else { return nil }
        let state =
            thread?.operatorMessages.first { $0.describes(message) }?.state ?? accepted
        let (text, tone) = describeDelivery(state)
        let replay = message.deduplicated ? "Already queued earlier under the same id; not sent twice. " : ""
        return (replay + text, tone)
    }
}
