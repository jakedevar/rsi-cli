import SwiftUI

/// One session's latest history page, its open decisions and their answer
/// forms. A failed refresh keeps what was shown and says what failed; while
/// open it reads again every minute, or on a backoff after a failure (#350).
struct SessionView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.theme) private var theme
    let project: WireProject
    let summary: SessionSummary

    static let every: Duration = .seconds(60)

    @State private var thread = HistoryThread()
    @State private var decisions: [Decision] = []
    @State private var freshness = Freshness()
    /// The latest decisions read failed; the decisions shown are older.
    @State private var decisionsError: RemoteError?
    @State private var refreshToken = 0
    @State private var disclosure = Disclosure()
    @State private var follow = TailFollow()

    var body: some View {
        let events = thread.events
        let turns = ChatTranscript.build(history: events)
        let rows = ChatTranscript.rows(turns)
        ScrollViewReader { proxy in
        List {
            FreshnessRow(freshness: freshness, every: Self.every)
            if !decisions.isEmpty || decisionsError != nil {
                Section("Waiting on you") {
                    if let decisionsError {
                        Text("\(decisionsError.headline) · decisions not refreshed")
                            .font(.caption).foregroundStyle(theme.warning.color)
                    }
                    ForEach(decisions) { DecisionRowView(decision: $0) }
                }
                .themedRow()
            }
            Section("Answer") {
                AnswerSectionView(
                    project: project, sessionId: summary.id, decisions: decisions,
                    refreshToken: refreshToken)
            }
            .themedRow()
            Section("Conversation") {
                if events.isEmpty, freshness.error == nil {
                    Text("No activity yet").foregroundStyle(.secondary)
                }
                if thread.canLoadOlder {
                    Button {
                        Task { await showOlder(proxy, firstRow: rows.first?.id) }
                    } label: {
                        HStack(spacing: 8) {
                            Text("Load older")
                            if thread.loadingOlder { ProgressView().controlSize(.small) }
                        }
                        .font(.footnote.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 4)
                    }
                    .buttonStyle(.bordered)
                    .disabled(thread.loadingOlder)
                    .listRowSeparator(.hidden)
                    .listRowBackground(Color.clear)
                }
                if let error = thread.olderError {
                    Text("\(error.headline) · older history not loaded")
                        .font(.caption).foregroundStyle(theme.warning.color)
                        .listRowBackground(Color.clear)
                }
                ForEach(rows) { row in
                    RowView(
                        row: row, assistantName: "Agent", disclosure: disclosure,
                        toggle: { id in withAnimation(.snappy(duration: 0.2)) { disclosure.toggle(id) } })
                        .listRowSeparator(.hidden)
                        .listRowBackground(Color.clear)
                        .listRowInsets(EdgeInsets(top: 0, leading: 16, bottom: 0, trailing: 16))
                        .id(row.id)
                }
            }
            if let error = freshness.error {
                Section { Text(error.message).foregroundStyle(theme.warning.color) }.themedRow()
            }
        }
        .themedList()
        .navigationTitle(summary.ownTitle.isEmpty ? "Session" : summary.ownTitle)
        .navigationBarTitleDisplayMode(.inline)
        .onScrollGeometryChange(for: ScrollEdge.self) { geometry in
            ScrollEdge(
                distance: Double(geometry.contentSize.height - geometry.visibleRect.maxY),
                height: Double(geometry.contentSize.height))
        } action: { _, edge in
            follow.scrolled(distanceFromBottom: edge.distance, contentHeight: edge.height)
        }
        .overlay(alignment: .bottomTrailing) {
            if !follow.atBottom, let last = rows.last {
                Button {
                    withAnimation(.easeOut(duration: 0.25)) { proxy.scrollTo(last.id, anchor: .bottom) }
                } label: {
                    Label("Latest", systemImage: "arrow.down")
                        .font(.caption.bold())
                        .padding(.horizontal, 12).padding(.vertical, 10)
                        .background(theme.elevated.color, in: Capsule())
                        .overlay(Capsule().strokeBorder(theme.border.color.opacity(0.6)))
                        .shadow(radius: 3, y: 1)
                }
                .buttonStyle(.plain)
                .padding(12)
            }
        }
        .refreshable { await load() }
        .task(id: model.refreshKey) { await keepFresh() }
        }
    }

    private func keepFresh() async {
        while !Task.isCancelled {
            if freshness.isStale(AppModel.reuseFor, now: Date()) { await load() }
            do {
                try await Task.sleep(for: freshness.wait(every: Self.every, now: Date()))
            } catch {
                return
            }
        }
    }

    /// Read the next older page with the cursor the hub served, then put the
    /// reader back on the row they were at the top of, so the history opens
    /// above instead of moving the view.
    private func showOlder(_ proxy: ScrollViewProxy, firstRow: String?) async {
        guard let client = model.client, let cursor = thread.olderCursor, !thread.loadingOlder else { return }
        thread.loadingOlder = true
        let result = await attempt {
            try await client.history(project: project.id, session: summary.id, cursor: cursor)
        }
        switch result {
        case .success(let page):
            thread.appendOlderPage(page, cursor: cursor)
            try? await Task.sleep(for: .milliseconds(50))
            if let firstRow { proxy.scrollTo(firstRow, anchor: .top) }
        case .failure(let error):
            // A refused cursor starts the thread over from the newest page.
            if thread.olderFailed(error, cursor: cursor) { await load() }
        }
    }

    private func load() async {
        guard let client = model.client else {
            freshness.failed(.notConfigured, at: Date())
            return
        }
        let projectId = project.id
        let sessionId = summary.id
        async let history = attempt { try await client.history(project: projectId, session: sessionId) }
        async let open = attempt { try await client.decisions(project: projectId, session: sessionId) }
        let (historyResult, decisionsResult) = await (history, open)
        switch decisionsResult {
        case .success(let list):
            decisions = list.items.filter(\.isOpen)
            decisionsError = nil
        case .failure(.cancelled):
            break
        case .failure(let error):
            decisionsError = error
        }
        switch historyResult {
        case .success(let page):
            thread.mergeFirstPage(page)
            freshness.succeeded(at: Date())
            refreshToken += 1
        case .failure(let error):
            freshness.failed(error, at: Date())
        }
    }
}

/// One read as a result; any error that is not a `RemoteError` is an unreadable reply.
private func attempt<T: Sendable>(
    _ read: @Sendable () async throws -> T
) async -> Result<T, RemoteError> {
    do {
        return .success(try await read())
    } catch let error as RemoteError {
        return .failure(error)
    } catch {
        return .failure(.invalidResponse("reply"))
    }
}

struct DecisionRowView: View {
    @Environment(\.theme) private var theme
    let decision: Decision

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(decision.title).font(.subheadline.bold()).foregroundStyle(theme.waiting.color)
            if !decision.detail.isEmpty {
                MarkdownText(text: decision.detail, font: .caption)
            }
            ForEach(Array(decision.questions.enumerated()), id: \.offset) { _, question in
                VStack(alignment: .leading, spacing: 2) {
                    if let header = question.header, !header.isEmpty {
                        Text(header).font(.caption.bold()).foregroundStyle(theme.label.color)
                    }
                    MarkdownText(text: question.question, font: .caption)
                    ForEach(question.options, id: \.label) { option in
                        Text("• \(option.label)").font(.caption2).foregroundStyle(.secondary)
                    }
                }
            }
        }
    }
}
