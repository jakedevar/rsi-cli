import Foundation
import Observation

/// The phone-side state of one answer to one waiting decision. It lives outside
/// the views, so a refresh keeps what was typed, which option was picked and
/// which idempotency key a possibly-delivered answer went out under.
struct AnswerDraft: Sendable, Equatable {
    enum Status: Sendable, Equatable {
        case editing, sending, receipt, unknown
    }

    let projectId: String
    let sessionId: String
    let decisionKey: String
    let pending: Bool
    let rowVersion: String?
    let digest: String
    let kind: TargetKind
    var title: String
    var detail: String
    var options: [String]
    var multiSelect: Bool

    var text = ""
    /// Options picked so far (multi-select composes `text` from them).
    var picks: [String] = []
    var idempotencyKey: String
    var status: Status = .editing
    var error: AnswerError?
    /// Why an earlier draft of this decision was discarded.
    var notice = ""
    var sentRequest: AnswerRequest?
    var sentText: String?
    var receipt: AnswerReceipt?

    /// What the form shows for this draft, and which controls apply.
    var view: DraftView {
        switch status {
        case .sending:
            return DraftView(tone: .busy, message: "Sending…", canEdit: false, canSend: false, canRetry: false, canDismiss: false)
        case .receipt:
            switch receipt {
            case .pending(let pending):
                let described = describePendingReceipt(pending)
                return DraftView(tone: described.tone, message: described.message, canEdit: false, canSend: false, canRetry: false, canDismiss: true)
            case .delivered(let key, let deduplicated):
                let receiptText = key.isEmpty ? "" : " Receipt \(key)."
                let again = deduplicated ? " This answer had already been recorded." : ""
                return DraftView(
                    tone: .ok,
                    message: "Sent. Your RSI host accepted the answer.\(receiptText)\(again) The decision leaves this list once the host moves on.",
                    canEdit: false, canSend: false, canRetry: false, canDismiss: true)
            case nil:
                return DraftView(tone: .ok, message: "Sent.", canEdit: false, canSend: false, canRetry: false, canDismiss: true)
            }
        case .unknown:
            let base = error?.message ?? "The outcome is unknown."
            return DraftView(
                tone: .warn,
                message: "\(base) Your answer is locked. Retry sends the same answer under the same key and cannot be applied twice.",
                canEdit: false, canSend: false, canRetry: true, canDismiss: false)
        case .editing:
            if let error {
                return DraftView(tone: .error, message: error.message, canEdit: true, canSend: true, canRetry: false, canDismiss: false)
            }
            if !notice.isEmpty {
                return DraftView(tone: .warn, message: notice, canEdit: true, canSend: true, canRetry: false, canDismiss: false)
            }
            return DraftView(tone: .idle, message: "", canEdit: true, canSend: true, canRetry: false, canDismiss: false)
        }
    }
}

struct DraftView: Sendable, Equatable {
    let tone: AnswerTone
    let message: String
    let canEdit: Bool
    let canSend: Bool
    let canRetry: Bool
    let canDismiss: Bool
}

/// All drafts, keyed by session and decision. Mirrors remote/web/state.js
/// `createDraftStore` and `submitDraft`.
@MainActor
@Observable
final class AnswerDrafts {
    private var drafts: [String: AnswerDraft] = [:]
    /// A discarded draft leaves one notice for the next draft of that decision.
    private var notices: [String: String] = [:]
    @ObservationIgnored private let newKey: @MainActor () -> String

    init(newKey: @escaping @MainActor () -> String = { UUID().uuidString.lowercased() }) {
        self.newKey = newKey
    }

    static func key(_ sessionId: String, _ decisionKey: String) -> String {
        "\(sessionId)\u{0}\(decisionKey)"
    }

    var count: Int { drafts.count }

    func draft(session: String, decision: String) -> AnswerDraft? {
        drafts[Self.key(session, decision)]
    }

    private func inFlight(_ draft: AnswerDraft) -> Bool { draft.status != .editing }

    /// The draft for a target. One started against another row version or
    /// digest is discarded: the decision changed under it, so the typed text
    /// no longer answers what is being asked. A sent answer keeps its draft so
    /// its outcome stays visible.
    @discardableResult
    func ensure(
        project: String, session: String, target: AnswerTarget, multiSelect: Bool = false
    ) -> AnswerDraft {
        let key = Self.key(session, target.decisionKey)
        if var existing = drafts[key] {
            if inFlight(existing) {
                return existing
            }
            if existing.rowVersion == target.rowVersion && existing.digest == target.digest {
                existing.title = target.title
                existing.detail = target.detail
                existing.options = target.options
                existing.multiSelect = multiSelect
                drafts[key] = existing
                return existing
            }
            notices[key] =
                "This decision changed while you were looking at it. Review it and answer again; your earlier draft was discarded."
        }
        var fresh = AnswerDraft(
            projectId: project, sessionId: session, decisionKey: target.decisionKey,
            pending: target.pending, rowVersion: target.rowVersion, digest: target.digest,
            kind: target.kind, title: target.title, detail: target.detail,
            options: target.options, multiSelect: multiSelect, idempotencyKey: newKey())
        fresh.notice = notices.removeValue(forKey: key) ?? ""
        drafts[key] = fresh
        return fresh
    }

    /// Drop idle drafts whose decision is no longer waiting, and make a draft
    /// for each target. `targets` is a fresh, successful read: never call this
    /// with a failed one.
    func sync(
        project: String, session: String, targets: [AnswerTarget],
        multiSelect: (AnswerTarget) -> Bool = { _ in false }
    ) {
        let present = Set(targets.map(\.decisionKey))
        for (key, draft) in drafts
        where draft.sessionId == session && !present.contains(draft.decisionKey)
            && !inFlight(draft)
        {
            drafts.removeValue(forKey: key)
        }
        for target in targets where target.isAnswerable && target.receipt == nil {
            ensure(project: project, session: session, target: target, multiSelect: multiSelect(target))
        }
    }

    private func update(_ session: String, _ decision: String, _ change: (inout AnswerDraft) -> Void) {
        let key = Self.key(session, decision)
        guard var draft = drafts[key] else { return }
        change(&draft)
        drafts[key] = draft
    }

    func setText(session: String, decision: String, _ text: String) {
        update(session, decision) { draft in
            guard !inFlight(draft) else { return }
            draft.text = text
            draft.picks = []
            draft.error = nil
            draft.notice = ""
            // The daemon replays a key only for the identical answer, so a
            // different text never reuses a key that already went out.
            if let sent = draft.sentText, sent != text {
                draft.idempotencyKey = newKey()
                draft.sentRequest = nil
                draft.sentText = nil
            }
        }
    }

    /// Tap an option: it replaces the answer, or toggles in a multi-select.
    func pick(session: String, decision: String, option: String) {
        guard let draft = draft(session: session, decision: decision), !inFlight(draft) else { return }
        var picks = draft.picks
        if draft.multiSelect {
            if let index = picks.firstIndex(of: option) { picks.remove(at: index) } else { picks.append(option) }
        } else {
            picks = [option]
        }
        let text = picks.joined(separator: ", ")
        setText(session: session, decision: decision, text)
        update(session, decision) { $0.picks = picks }
    }

    /// Freeze the request for this attempt, or resend the frozen one after an
    /// unknown outcome. Returns the problem when nothing may be sent.
    func begin(session: String, decision: String, fence: AnswerFence?) -> Result<AnswerRequest, AnswerError> {
        guard let draft = draft(session: session, decision: decision) else {
            return .failure(AnswerError(.invalid, "This decision is no longer waiting. Refresh."))
        }
        if draft.status == .sending { return .failure(AnswerError(.invalid, "Already sending.")) }
        if draft.status == .unknown, let frozen = draft.sentRequest {
            update(session, decision) { $0.status = .sending; $0.error = nil }
            return .success(frozen)
        }
        if draft.status == .receipt { return .failure(AnswerError(.invalid, "Already sent.")) }
        if let problem = answerProblem(draft.text) {
            return .failure(AnswerError(.invalid, problem))
        }
        if !draft.pending && fence == nil {
            return .failure(AnswerError(.invalid, "The answer fence is not available. Refresh and try again."))
        }
        let request = AnswerRequest(
            decisionKey: draft.decisionKey, pending: draft.pending, rowVersion: draft.rowVersion,
            digest: draft.digest, fence: draft.pending ? nil : fence, answer: draft.text,
            idempotencyKey: draft.idempotencyKey)
        update(session, decision) {
            $0.sentRequest = request
            $0.sentText = $0.text
            $0.status = .sending
            $0.error = nil
        }
        return .success(request)
    }

    func settleOk(session: String, decision: String, receipt: AnswerReceipt) {
        update(session, decision) {
            $0.status = .receipt
            $0.receipt = receipt
            $0.error = nil
        }
    }

    func settleError(session: String, decision: String, error: AnswerError) {
        let key = Self.key(session, decision)
        if error.kind == .stale {
            // The decision changed: discard the draft; the next targets read
            // makes a new one that carries this notice.
            notices[key] =
                "This decision changed. Refresh shows its current state; your draft was discarded."
            drafts.removeValue(forKey: key)
            return
        }
        update(session, decision) {
            $0.error = error
            $0.status = error.kind == .unknown ? .unknown : .editing
        }
    }

    /// Done with a delivered answer; the next read shows what happened.
    func dismiss(session: String, decision: String) {
        drafts.removeValue(forKey: Self.key(session, decision))
    }

    /// Send one draft through the client and record the outcome. Never throws
    /// for an expected refusal; returns the draft's resulting status.
    @discardableResult
    func submit(
        client: RemoteClient, session: String, decision: String, fence: AnswerFence?
    ) async -> AnswerDraft.Status? {
        let request: AnswerRequest
        switch begin(session: session, decision: decision, fence: fence) {
        case .failure(let problem):
            update(session, decision) { $0.error = problem }
            return draft(session: session, decision: decision)?.status
        case .success(let frozen):
            request = frozen
        }
        guard let project = draft(session: session, decision: decision)?.projectId else { return nil }
        do {
            let receipt = try await client.answer(project: project, session: session, request: request)
            settleOk(session: session, decision: decision, receipt: receipt)
        } catch let error as AnswerError {
            settleError(session: session, decision: decision, error: error)
        } catch {
            settleError(
                session: session, decision: decision,
                error: AnswerError(.unexpected, "The answer could not be sent."))
        }
        return draft(session: session, decision: decision)?.status
    }
}
