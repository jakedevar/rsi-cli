import SwiftUI

/// The answer forms for one session's waiting decisions: it reads the
/// gateway's decision-targets and shows one form per answerable target. When
/// answers are off (403/404) it says so and the session stays read-only.
struct AnswerSectionView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.theme) private var theme
    let project: WireProject
    let sessionId: String
    /// The read-only decisions of the same session, used to tell multi-select
    /// questions apart (the targets list carries only option labels).
    var decisions: [Decision] = []
    /// Changes when the parent refreshes, so the targets are read again.
    var refreshToken = 0

    @State private var outcome: TargetsOutcome?

    var body: some View {
        Group {
            switch outcome {
            case nil:
                EmptyView()
            case .off:
                Text("Answers are turned off on this host. This session is read-only here.")
                    .font(.caption).foregroundStyle(.secondary)
            case .failed(let error):
                VStack(alignment: .leading, spacing: 6) {
                    Text("Could not load answer forms. \(error.message)")
                        .font(.caption).foregroundStyle(theme.error.color)
                    Button("Retry") { Task { await reload() } }
                        .font(.caption)
                }
            case .ok(let targets):
                if targets.truncated {
                    Text("More decisions are waiting than this list shows.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                ForEach(targets.items) { target in
                    AnswerFormView(
                        project: project.id, sessionId: sessionId, target: target,
                        fence: targets.fence, onChanged: { await reload() })
                }
            }
        }
        .task(id: refreshToken) { await reload() }
    }

    private func multiSelect(_ target: AnswerTarget) -> Bool {
        decisions.first { $0.id == target.decisionKey }?.questions.contains(where: \.multiSelect) ?? false
    }

    private func reload() async {
        guard let client = model.client else { return }
        let result = await client.decisionTargets(project: project.id, session: sessionId)
        // The row went away mid-read: keep what is shown.
        if case .failed(.cancelled) = result { return }
        if case .ok(let targets) = result {
            model.drafts.sync(
                project: project.id, session: sessionId, targets: targets.items,
                multiSelect: multiSelect)
        }
        outcome = result
    }
}

/// One decision's answer form: option buttons (single or multi-select) and, for
/// a question, free text, plus Send and the delivery state.
struct AnswerFormView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.theme) private var theme
    let project: String
    let sessionId: String
    let target: AnswerTarget
    let fence: AnswerFence?
    /// Called after a send settles, to read the targets again.
    let onChanged: () async -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(target.kind == .question ? "Question" : "Approval")
                    .font(.caption2.bold()).foregroundStyle(theme.label.color)
                Text(target.title).font(.subheadline.bold())
            }
            if !target.detail.isEmpty {
                MarkdownText(text: target.detail, font: .caption)
            }
            if let receipt = target.receipt {
                Text(describePendingReceipt(receipt).message)
                    .font(.caption).foregroundStyle(.secondary)
            } else if let draft = model.drafts.draft(session: sessionId, decision: target.decisionKey) {
                form(draft)
            } else {
                Text("No answer choices were offered for this approval. Answer it on your RSI host.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 4)
    }

    @ViewBuilder
    private func form(_ draft: AnswerDraft) -> some View {
        let state = draft.view
        if !draft.options.isEmpty {
            ForEach(draft.options, id: \.self) { option in
                let picked = draft.picks.contains(option) || (!draft.multiSelect && draft.text == option)
                Button {
                    model.drafts.pick(session: sessionId, decision: target.decisionKey, option: option)
                } label: {
                    HStack {
                        Image(systemName: picked
                            ? (draft.multiSelect ? "checkmark.square.fill" : "largecircle.fill.circle")
                            : (draft.multiSelect ? "square" : "circle"))
                        Text(option).multilineTextAlignment(.leading)
                        Spacer(minLength: 0)
                    }
                }
                .buttonStyle(.bordered)
                .disabled(!state.canEdit)
                .accessibilityAddTraits(picked ? .isSelected : [])
            }
        }
        if target.kind == .question {
            TextField(
                draft.options.isEmpty ? "Your answer" : "Or write your own answer",
                text: Binding(
                    get: { draft.text },
                    set: { model.drafts.setText(session: sessionId, decision: target.decisionKey, $0) }),
                axis: .vertical
            )
            .lineLimit(2...6)
            .textFieldStyle(.roundedBorder)
            .disabled(!state.canEdit)
            Text("\(draft.text.utf8.count) / \(maxAnswerBytes) bytes")
                .font(.caption2)
                .foregroundStyle((draft.text.utf8.count > maxAnswerBytes ? theme.error : theme.mutedText).color)
        }
        actions(draft, state)
        if !state.message.isEmpty {
            Text(state.message).font(.caption).foregroundStyle(theme.color(state.tone))
                .accessibilityLabel(state.message)
        }
    }

    @ViewBuilder
    private func actions(_ draft: AnswerDraft, _ state: DraftView) -> some View {
        HStack {
            if state.canRetry {
                Button("Retry same answer") { send() }.buttonStyle(.borderedProminent)
                Button("Refresh") { Task { await onChanged() } }
            } else if state.canDismiss {
                Button("Done") {
                    model.drafts.dismiss(session: sessionId, decision: target.decisionKey)
                    Task { await onChanged() }
                }
            } else {
                Button(draft.status == .sending ? "Sending…" : "Send answer") { send() }
                    .buttonStyle(.borderedProminent)
                    .disabled(!state.canSend || (!draft.pending && fence == nil)
                        || answerProblem(draft.text) != nil)
            }
            if draft.status == .sending { ProgressView() }
        }
    }

    private func send() {
        guard let client = model.client else { return }
        let session = sessionId
        let decision = target.decisionKey
        let fence = fence
        Task {
            await model.drafts.submit(client: client, session: session, decision: decision, fence: fence)
            let settled = model.drafts.draft(session: session, decision: decision)
            if settled == nil || settled?.status == .receipt {
                // Delivered or stale: read the targets again.
                await onChanged()
            }
        }
    }
}
