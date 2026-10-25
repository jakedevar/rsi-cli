import SwiftUI

/// The Manager pane of a project: the project's PM or the covering global
/// manager (gateway API v2).
struct ManagerChatView: View {
    @Environment(AppModel.self) private var model
    let project: WireProject
    @State private var chat: ManagerChat?

    var body: some View {
        Group {
            if let chat {
                ManagerChatContent(chat: chat)
            } else if model.host == nil {
                ContentUnavailableView(
                    "No RSI host", systemImage: "network.slash",
                    description: Text("Set the hub's Tailscale host name in Settings."))
            } else {
                ProgressView()
            }
        }
        .task { if chat == nil { chat = model.chat(project: project.id) } }
    }
}

private struct PollKey: Hashable {
    let role: SeatRole
    let seat: String?
    let active: Bool
}

struct ManagerChatContent: View {
    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    @Bindable var chat: ManagerChat

    var body: some View {
        VStack(spacing: 0) {
            Picker("Manager", selection: $chat.role) {
                ForEach(SeatRole.allCases, id: \.self) { Text($0.label).tag($0) }
            }
            .pickerStyle(.segmented)
            .padding(.horizontal)
            .padding(.bottom, 8)
            if let seat = chat.seat {
                SeatHeaderView(seat: chat.thread?.seatInfo ?? seat)
                    .padding(.horizontal).padding(.bottom, 8)
                Divider()
                ManagerTranscript(chat: chat)
            } else if chat.seats == nil {
                noSeats
            } else {
                ContentUnavailableView(
                    chat.role == .projectManager ? "No project manager" : "No global manager",
                    systemImage: "person.crop.circle.badge.questionmark",
                    description: Text(
                        chat.role == .projectManager
                            ? "This project has no appointed manager seat right now."
                            : "No global manager covers this project right now."))
            }
        }
        .themedList()
        // The composer stays at the bottom, above the keyboard and the tab bar.
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if chat.seat != nil { ComposerView(chat: chat) }
        }
        // Poll only while this pane is on screen and the app is active.
        .task(id: PollKey(role: chat.role, seat: chat.seat?.sessionId, active: scenePhase == .active)) {
            guard scenePhase == .active, chat.seat != nil else { return }
            await chat.poll()
        }
    }

    @ViewBuilder private var noSeats: some View {
        if let error = model.seatsError {
            ErrorView(error: error) { Task { await model.refreshProjects() } }
        } else if model.loadingProjects {
            ProgressView("Loading managers…")
        } else {
            ContentUnavailableView(
                "No manager seats", systemImage: "bubble.left.and.bubble.right",
                description: Text("The hub did not list this project's managers. Pull to refresh Projects."))
        }
    }
}

struct SeatHeaderView: View {
    @Environment(\.theme) private var theme
    let seat: ManagerSeat

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(seat.role == SeatRole.globalManager.rawValue ? "Global manager" : "Project manager")
                    .font(.headline)
                Spacer()
                if let badge = seat.pendingBadge {
                    PendingBadge(text: badge)
                }
            }
            Text(seat.summary).font(.caption).foregroundStyle(theme.model.color)
            Text("Last activity \(localTime(seat.lastActivityAt))")
                .font(.caption2).foregroundStyle(theme.time.color)
        }
    }
}

struct PendingBadge: View {
    @Environment(\.theme) private var theme
    let text: String

    var body: some View {
        Text(text)
            .font(.caption.bold())
            .padding(.horizontal, 8).padding(.vertical, 2)
            .background(theme.waiting.color, in: Capsule())
            .foregroundStyle(theme.onAccent.color)
            .accessibilityLabel("\(text) pending decisions")
    }
}

/// The compose bar: a rounded field and a round send button, pinned to the
/// bottom. A sent message leaves at once into the outbox (the chat shows it as
/// a pending bubble) and the hub gets it however long that takes (#505).
struct ComposerView: View {
    @Environment(\.theme) private var theme
    @Bindable var chat: ManagerChat

    var body: some View {
        let draft = chat.draft
        let bytes = draft.text.utf8.count
        let canSend = !chat.composeDisabled && messageProblem(draft.text) == nil
        VStack(alignment: .leading, spacing: 6) {
            if chat.messagingOff {
                MessagingOffNotice()
            }
            HStack(alignment: .bottom, spacing: 8) {
                TextField("Message the manager", text: $chat.draft.text, axis: .vertical)
                    .lineLimit(1...6)
                    .disabled(chat.composeDisabled)
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    .background(theme.elevated.color, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
                Button { chat.send() } label: {
                    Image(systemName: "arrow.up.circle.fill")
                        .font(.system(size: 34))
                        .foregroundStyle((canSend ? theme.primary : theme.disabled).color)
                }
                .buttonStyle(.plain)
                .disabled(!canSend)
                .accessibilityLabel("Send")
                .accessibilityHint("Saved on this phone and retried until the hub has it")
            }
            if bytes > maxMessageBytes * 4 / 5 {
                Text("\(bytes)/\(maxMessageBytes) bytes")
                    .font(.caption2)
                    .foregroundStyle((bytes > maxMessageBytes ? theme.error : theme.mutedText).color)
            }
            if let problem = draft.problem {
                Text(problem).font(.caption).foregroundStyle(theme.error.color)
            }
            if let problem = chat.outbox.storageProblem {
                Text(problem).font(.caption).foregroundStyle(theme.error.color)
            }
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(theme.chrome.color.ignoresSafeArea(edges: .bottom))
        .overlay(alignment: .top) { Divider() }
    }
}

/// The manager conversation: turns by role, the messages still on their way as
/// pending bubbles at the bottom.
struct ManagerTranscript: View {
    @Environment(\.theme) private var theme
    @Bindable var chat: ManagerChat

    var body: some View {
        let thread = chat.thread
        let items = thread?.items ?? []
        let turns = ChatTranscript.build(items: items)
        let pending = pendingOutgoing(
            chat.outgoing, conversation: items, deliveries: thread?.operatorMessages ?? [])
        ConversationScroll(
            turns: turns, assistantName: "Manager",
            delivery: { event in thread?.deliveryState(of: event) },
            pendingCount: pending.count,
            canLoadOlder: thread?.hasMore ?? false,
            loadingOlder: thread?.loadingOlder ?? false,
            loadOlder: { await chat.loadOlder() },
            header: {
                if let thread, let error = thread.error {
                    Label(error.message, systemImage: error.isUnreachable ? "wifi.exclamationmark" : "exclamationmark.triangle")
                        .font(.caption).foregroundStyle(theme.error.color)
                }
                if let error = thread?.olderError {
                    Text(error.message).font(.caption).foregroundStyle(theme.error.color)
                }
                if let thread, thread.loaded {
                    if items.isEmpty {
                        Text("No messages yet. Say something below.")
                            .foregroundStyle(.secondary).frame(maxWidth: .infinity)
                    }
                } else if thread?.error == nil {
                    ProgressView("Loading conversation…").frame(maxWidth: .infinity)
                }
            },
            footer: {
                ForEach(pending) { message in
                    OutboxBubble(chat: chat, message: message)
                }
            }
        )
        .refreshable { await chat.refresh() }
    }
}

/// A message written here that the conversation does not carry yet: queued,
/// sending, failed or just delivered, with what to do about it.
struct OutboxBubble: View {
    @Environment(\.theme) private var theme
    let chat: ManagerChat
    let message: OutboxMessage

    var body: some View {
        OutboxRow(chat: chat, message: message)
            .padding(12)
            .background(theme.elevated.color, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 18, style: .continuous)
                    .strokeBorder(
                        theme.color(describeOutbox(message).tone).opacity(0.6),
                        style: StrokeStyle(lineWidth: 1, dash: message.state == .delivered ? [] : [4, 3])))
            .padding(.leading, 48)
            .frame(maxWidth: .infinity, alignment: .trailing)
    }
}

struct OutboxRow: View {
    @Environment(\.theme) private var theme
    let chat: ManagerChat
    let message: OutboxMessage

    var body: some View {
        let shown = describeOutbox(message)
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                if message.state == .sending { ProgressView() }
                Text(shown.state).font(.caption.bold()).foregroundStyle(theme.color(shown.tone))
                Spacer()
                Text(message.createdAt.formatted(date: .omitted, time: .shortened)).font(.caption2).foregroundStyle(theme.time.color)
            }
            Text(message.text).font(.callout).lineLimit(4)
            if let reason = shown.reason {
                Text(reason).font(.caption).foregroundStyle(theme.color(shown.tone))
            }
            if message.state == .queued, let next = message.nextAttemptAt, message.failures > 0 {
                HStack(spacing: 4) {
                    Text("Next try in").font(.caption2).foregroundStyle(.secondary)
                    Text(next, style: .timer).font(.caption2.monospacedDigit()).foregroundStyle(.secondary)
                    Text("· try \(message.failures + 1)").font(.caption2).foregroundStyle(.secondary)
                }
            }
            if let note = shown.note {
                Text(note).font(.caption2).foregroundStyle(.secondary)
            }
            if let delivery = chat.delivery(of: message) {
                Text(delivery.text).font(.caption2).foregroundStyle(theme.color(delivery.tone))
            }
            if message.state == .queued || message.state == .failed {
                HStack {
                    Button("Discard", role: .destructive) { chat.outbox.discard(message.id) }
                    Spacer()
                    if message.state == .failed {
                        Button("Send again") { chat.outbox.sendAgain(message.id) }
                    } else if message.failures > 0 {
                        Button("Try now") { chat.outbox.retryNow(message.id) }
                    }
                }
                .buttonStyle(.bordered)
                .font(.caption)
            } else if message.state == .delivered {
                Button("Dismiss") { chat.outbox.discard(message.id) }
                    .buttonStyle(.plain).font(.caption2).foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .contain)
    }
}

/// The operator has not turned phone messaging on. A setting, not a fault.
struct MessagingOffNotice: View {
    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text("Messaging from the phone is off").font(.subheadline.weight(.semibold))
                Text("Turn it on in the hub's Remote settings (Settings → Integrations → Remote → Remote access, m), then send again. Reading the conversation still works.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        } icon: {
            Image(systemName: "bubble.left.and.exclamationmark.bubble.right").foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }
}
