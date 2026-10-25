import Foundation

// A session's history as loaded so far (#515, #508 item 4): the newest page
// the screen polls, plus the older pages the reader asked for with the
// `next_cursor` the hub serves. Pure, so the tests can drive it without a view.

struct HistoryThread: Sendable, Equatable {
    /// Every event loaded, oldest first by (sequence, id).
    private(set) var events: [HistoryEvent] = []
    /// `next_cursor` token of the oldest page loaded; nil once the oldest page
    /// is in. It only moves back: a poll of the newest page never replaces it.
    private(set) var olderCursor: String?
    private(set) var loaded = false
    var loadingOlder = false
    var olderError: RemoteError?
    /// The hub refused the cursor (it expired, or the history moved under it):
    /// the next first page replaces everything instead of merging.
    private var cursorLost = false

    var canLoadOlder: Bool { olderCursor != nil }

    private static func olderFirst(_ a: HistoryEvent, _ b: HistoryEvent) -> Bool {
        if a.sequence != b.sequence { return a.sequence < b.sequence }
        if let x = Int64(a.id), let y = Int64(b.id), x != y { return x < y }
        return a.id < b.id
    }

    /// Merge a freshly read newest page. Overlap keeps the older pages already
    /// loaded; a page that does not touch what is held (more than a page of new
    /// events arrived between reads) replaces it, so there is never a silent
    /// gap in the middle.
    mutating func mergeFirstPage(_ page: HistoryPageResult) {
        let fresh = page.items.sorted(by: Self.olderFirst)
        let known = Set(events.map(\.id))
        let touches = fresh.contains { known.contains($0.id) }
        let newer: Bool = {
            guard let oldestNew = fresh.first, let newestKnown = events.last else { return false }
            return Self.olderFirst(newestKnown, oldestNew)
        }()
        if !loaded || events.isEmpty || cursorLost || (!touches && newer && !fresh.isEmpty) {
            events = fresh
            olderCursor = page.nextCursor?.token
            cursorLost = false
            olderError = nil
        } else {
            let replaced = Set(fresh.map(\.id))
            events = (events.filter { !replaced.contains($0.id) } + fresh).sorted(by: Self.olderFirst)
        }
        loaded = true
    }

    /// Append the older page read with `cursor`. A page for a cursor that is no
    /// longer the thread's (a poll replaced the thread meanwhile) is dropped.
    mutating func appendOlderPage(_ page: HistoryPageResult, cursor: String) {
        loadingOlder = false
        guard olderCursor == cursor else { return }
        let known = Set(events.map(\.id))
        events = (events + page.items.filter { !known.contains($0.id) }).sorted(by: Self.olderFirst)
        olderCursor = page.nextCursor?.token
        olderError = nil
    }

    /// Reading the older page failed. A refusal from the hub (not a lost
    /// connection) means the cursor is no good any more; the next first page
    /// starts the thread over. Returns whether the caller should read it now.
    @discardableResult
    mutating func olderFailed(_ error: RemoteError, cursor: String) -> Bool {
        loadingOlder = false
        guard error != .cancelled, olderCursor == cursor else { return false }
        olderError = error
        if case .server = error {
            cursorLost = true
            return true
        }
        return false
    }
}
