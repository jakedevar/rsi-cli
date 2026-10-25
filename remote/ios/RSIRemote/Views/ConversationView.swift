import SwiftUI

// The chat-app look of a conversation (#502): the operator's messages as
// bubbles, the assistant's output as flowing text, tool calls and thinking
// folded until asked for. The turn model is in Model/Turns.swift.

/// How far the reader is from the end, and how tall the content is.
struct ScrollEdge: Equatable {
    let distance: Double
    let height: Double
}

/// What the tail follower watches: the newest event and how many there are.
struct TailMark: Equatable {
    let newestID: String?
    let eventCount: Int

    init(_ turns: [Turn]) {
        let newest = ChatTranscript.newest(turns)
        newestID = newest.id
        eventCount = newest.count
    }
}

/// One row of a turn. Messages in markdown, tool calls and thinking folded; the
/// state of what is open lives with the caller (`disclosure`), so a row leaving
/// the lazy list and coming back keeps what the reader opened.
struct RowView: View {
    @Environment(\.theme) private var theme
    let row: ChatRow
    let assistantName: String
    let disclosure: Disclosure
    let toggle: (String) -> Void
    /// The hub's delivery state of an operator message, when it has one.
    var delivery: (ChatEvent) -> String? = { _ in nil }

    var body: some View {
        content
            .padding(.top, row.startsTurn ? 18 : 8)
            .frame(maxWidth: .infinity, alignment: alignment)
    }

    private var alignment: Alignment {
        switch row.role {
        case .user: .trailing
        case .assistant: .leading
        case .system: .center
        }
    }

    @ViewBuilder private var content: some View {
        switch row.kind {
        case .header:
            HStack(spacing: 6) {
                Image(systemName: "sparkles").font(.caption).foregroundStyle(theme.assistantRole.color)
                Text(assistantName).font(.caption.bold()).foregroundStyle(theme.assistantRole.color)
                Text(localTime(row.createdAt)).font(.caption2).foregroundStyle(theme.time.color)
            }
            .accessibilityElement(children: .combine)
        case .stamp:
            Text(localTime(row.createdAt)).font(.caption2).foregroundStyle(theme.time.color)
        case .part(let part):
            switch row.role {
            case .user: userPart(part)
            case .assistant: assistantPart(part)
            case .system: systemPart(part)
            }
        }
    }

    @ViewBuilder private func userPart(_ part: TurnPart) -> some View {
        if case .text(let event) = part {
            VStack(alignment: .trailing, spacing: 2) {
                MarkdownText(text: event.text, font: .body)
                    .textSelection(.enabled)
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    .background(
                        theme.elevated.color,
                        in: RoundedRectangle(cornerRadius: 18, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 18, style: .continuous)
                            .strokeBorder(theme.userRole.color.opacity(0.45), lineWidth: 1))
                if event.truncated {
                    Text("Cut at 4096 bytes; the full text is on the hub.")
                        .font(.caption2).foregroundStyle(.secondary)
                }
                if let state = delivery(event) {
                    let shown = describeDelivery(state)
                    Text(shown.text).font(.caption2).foregroundStyle(theme.color(shown.tone))
                }
            }
            .padding(.leading, 48)
            .accessibilityElement(children: .contain)
            .accessibilityLabel("You")
        }
    }

    @ViewBuilder private func systemPart(_ part: TurnPart) -> some View {
        if case .text(let event) = part {
            MarkdownText(text: event.text, font: .caption.italic())
                .foregroundStyle(theme.systemEvent.color)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 12)
        }
    }

    @ViewBuilder private func assistantPart(_ part: TurnPart) -> some View {
        switch part {
        case .text(let event):
            VStack(alignment: .leading, spacing: 2) {
                MarkdownText(text: event.text, font: .body)
                    .textSelection(.enabled)
                if event.truncated {
                    Text("Cut at 4096 bytes; the full text is on the hub.")
                        .font(.caption2).foregroundStyle(.secondary)
                }
            }
            .padding(.trailing, 16)
        case .thinking(let event):
            FoldRow(
                title: "Thinking", icon: "brain", tint: theme.thinking.color,
                isOpen: disclosure.isOpen(part.id), toggle: { toggle(part.id) }
            ) {
                Text(event.text + (event.truncated ? " …" : ""))
                    .font(.callout.italic()).foregroundStyle(theme.mutedText.color)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        case .tools(_, let calls):
            FoldRow(
                title: toolSummary(calls), icon: "wrench.and.screwdriver", tint: theme.toolName.color,
                busy: calls.contains { $0.running },
                isOpen: disclosure.isOpen(part.id), toggle: { toggle(part.id) }
            ) {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(calls) { call in
                        ToolCallView(
                            call: call, isOpen: disclosure.isOpen("call:" + call.id),
                            toggle: { toggle("call:" + call.id) })
                    }
                }
            }
        }
    }
}

/// A folded row: a title with a chevron; the content shows when it is open.
struct FoldRow<Content: View>: View {
    @Environment(\.theme) private var theme
    let title: String
    let icon: String
    let tint: Color
    var busy = false
    let isOpen: Bool
    let toggle: () -> Void
    let content: Content

    init(
        title: String, icon: String, tint: Color, busy: Bool = false, isOpen: Bool,
        toggle: @escaping () -> Void, @ViewBuilder content: () -> Content
    ) {
        self.title = title
        self.icon = icon
        self.tint = tint
        self.busy = busy
        self.isOpen = isOpen
        self.toggle = toggle
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button(action: toggle) {
                HStack(spacing: 8) {
                    Image(systemName: icon).font(.caption).foregroundStyle(tint)
                    Text(title).font(.footnote).lineLimit(1).foregroundStyle(theme.mutedText.color)
                    if busy { ProgressView().controlSize(.mini) }
                    Spacer(minLength: 4)
                    Image(systemName: "chevron.right")
                        .font(.caption2.weight(.semibold)).foregroundStyle(theme.dimText.color)
                        .rotationEffect(.degrees(isOpen ? 90 : 0))
                }
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(theme.panel.color, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityValue(isOpen ? "expanded" : "collapsed")
            .accessibilityHint(isOpen ? "Double tap to collapse" : "Double tap to expand")
            if isOpen {
                content.padding(.leading, 8)
            }
        }
    }
}

/// One tool call inside an open group: its name, folded to the first line of
/// what was asked; opened, the whole request and result.
struct ToolCallView: View {
    @Environment(\.theme) private var theme
    let call: ToolCall
    let isOpen: Bool
    let toggle: () -> Void

    var body: some View {
        let request = call.use?.text ?? ""
        let preview = clippedPreview(request, maxLines: 1, maxCharacters: 80)
        VStack(alignment: .leading, spacing: 4) {
            Button(action: toggle) {
                HStack(spacing: 6) {
                    Text(call.name).font(.caption.bold()).foregroundStyle(theme.toolName.color)
                    if !isOpen, !preview.text.isEmpty {
                        Text(preview.text + (preview.cut ? " …" : ""))
                            .font(.caption.monospaced()).lineLimit(1)
                            .foregroundStyle(theme.mutedText.color)
                    }
                    if call.running { ProgressView().controlSize(.mini) }
                    Spacer(minLength: 4)
                    Image(systemName: isOpen ? "chevron.up" : "chevron.down")
                        .font(.caption2).foregroundStyle(theme.dimText.color)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityValue(isOpen ? "expanded" : "collapsed")
            if isOpen {
                if let use = call.use {
                    codeBlock(use.text + (use.truncated ? " …" : ""))
                }
                if let result = call.result {
                    Text("Result").font(.caption2.bold()).foregroundStyle(theme.label.color)
                    codeBlock(result.text + (result.truncated ? " …" : ""))
                } else if call.running {
                    Text("Waiting for the result…").font(.caption2).foregroundStyle(.secondary)
                }
            }
        }
    }

    private func codeBlock(_ text: String) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            Text(text.isEmpty ? "(empty)" : text)
                .font(.caption.monospaced()).foregroundStyle(theme.codeFg.color)
                .textSelection(.enabled)
                .fixedSize(horizontal: true, vertical: false)
                .padding(8)
        }
        .background(theme.codeBg.color, in: RoundedRectangle(cornerRadius: 6))
    }
}

/// The scrolling conversation: turns oldest to newest in a lazy stack, the
/// view at the newest end when it opens, following new output while the reader
/// is at the bottom, and a jump to the latest when they are not.
struct ConversationScroll<Header: View, Footer: View>: View {
    @Environment(\.theme) private var theme
    let turns: [Turn]
    let assistantName: String
    var delivery: (ChatEvent) -> String? = { _ in nil }
    /// Messages pending below the conversation (the outbox); a new one scrolls to it.
    var pendingCount = 0
    var canLoadOlder = false
    var loadingOlder = false
    var loadOlder: () async -> Void = {}
    let header: Header
    let footer: Footer

    init(
        turns: [Turn], assistantName: String, delivery: @escaping (ChatEvent) -> String? = { _ in nil },
        pendingCount: Int = 0, canLoadOlder: Bool = false, loadingOlder: Bool = false,
        loadOlder: @escaping () async -> Void = {},
        @ViewBuilder header: () -> Header, @ViewBuilder footer: () -> Footer
    ) {
        self.turns = turns
        self.assistantName = assistantName
        self.delivery = delivery
        self.pendingCount = pendingCount
        self.canLoadOlder = canLoadOlder
        self.loadingOlder = loadingOlder
        self.loadOlder = loadOlder
        self.header = header()
        self.footer = footer()
    }

    @State private var disclosure = Disclosure()
    @State private var follow = TailFollow()
    @State private var opened = false
    /// Bumped when the reader touches the list: a pending settle then stands down.
    @State private var touches = 0
    @State private var position = ScrollPosition(edge: .bottom)

    private static var bottomID: String { "conversation-bottom" }

    private var rows: [ChatRow] { ChatTranscript.rows(turns) }

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if canLoadOlder {
                        Button {
                            Task { await showOlder(proxy) }
                        } label: {
                            HStack(spacing: 8) {
                                Text("Load older")
                                if loadingOlder { ProgressView().controlSize(.small) }
                            }
                            .font(.footnote.weight(.semibold))
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 8)
                        }
                        .buttonStyle(.bordered)
                        .disabled(loadingOlder)
                    }
                    header
                    ForEach(rows) { row in
                        RowView(
                            row: row, assistantName: assistantName, disclosure: disclosure,
                            toggle: { id in withAnimation(.snappy(duration: 0.2)) { disclosure.toggle(id) } },
                            delivery: delivery)
                    }
                    footer
                    Color.clear.frame(height: 1).id(Self.bottomID)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 12)
            }
            .scrollPosition($position)
            .defaultScrollAnchor(.bottom, for: .initialOffset)
            .scrollDismissesKeyboard(.interactively)
            .onScrollPhaseChange { _, phase in
                if phase == .interacting { touches += 1 }
            }
            .onScrollGeometryChange(for: ScrollEdge.self) { geometry in
                ScrollEdge(
                    distance: Double(geometry.contentSize.height - geometry.visibleRect.maxY),
                    height: Double(geometry.contentSize.height))
            } action: { _, edge in
                follow.scrolled(distanceFromBottom: edge.distance, contentHeight: edge.height)
            }
            .onChange(of: TailMark(turns), initial: true) { _, mark in
                let animated = opened
                opened = true
                if follow.update(newestID: mark.newestID, eventCount: mark.eventCount) {
                    scrollToLatest(proxy, animated: animated)
                    settleAtLatest()
                }
            }
            .onChange(of: pendingCount) { old, new in
                if new > old {
                    follow.jumpedToLatest()
                    scrollToLatest(proxy, animated: true)
                }
            }
            .overlay(alignment: .bottomTrailing) {
                if !follow.atBottom {
                    Button {
                        follow.jumpedToLatest()
                        scrollToLatest(proxy, animated: true)
                    } label: {
                        HStack(spacing: 6) {
                            if follow.unseen > 0 { Text("\(follow.unseen) new").font(.caption.bold()) }
                            Image(systemName: "arrow.down")
                        }
                        .padding(.horizontal, 12).padding(.vertical, 10)
                        .background(theme.elevated.color, in: Capsule())
                        .overlay(Capsule().strokeBorder(theme.border.color.opacity(0.6)))
                        .shadow(radius: 3, y: 1)
                    }
                    .buttonStyle(.plain)
                    .padding(12)
                    .accessibilityLabel(follow.unseen > 0 ? "Jump to latest, \(follow.unseen) new" : "Jump to latest")
                    .transition(.scale.combined(with: .opacity))
                }
            }
            .animation(.snappy(duration: 0.2), value: follow.atBottom)
        }
    }

    /// Load an older page, then put the reader back on the turn they were at the
    /// top of, so the new history opens above instead of moving the view.
    private func showOlder(_ proxy: ScrollViewProxy) async {
        let anchor = rows.first { if case .part = $0.kind { true } else { false } }?.id
        await loadOlder()
        try? await Task.sleep(for: .milliseconds(50))
        if let anchor { proxy.scrollTo(anchor, anchor: .top) }
    }

    /// A scroll to the end lands before the new rows are laid out, and rows not
    /// drawn yet have only estimated heights; look again once they are measured,
    /// unless the reader has touched the list meanwhile.
    private func settleAtLatest() {
        let started = touches
        Task { @MainActor in
            for delay in [100, 150, 250, 400, 700] {
                try? await Task.sleep(for: .milliseconds(delay))
                guard touches == started else { return }
                position.scrollTo(edge: .bottom)
            }
        }
    }

    private func scrollToLatest(_ proxy: ScrollViewProxy, animated: Bool) {
        // The edge, not a row id: rows not drawn yet have only estimated heights.
        if animated {
            withAnimation(.easeOut(duration: 0.25)) { position.scrollTo(edge: .bottom) }
        } else {
            position.scrollTo(edge: .bottom)
        }
    }
}
