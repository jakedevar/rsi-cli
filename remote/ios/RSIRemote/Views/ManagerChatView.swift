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
                List {
                    Section {
                        SeatHeaderView(seat: chat.thread?.seatInfo ?? seat)
                        ComposerView(chat: chat)
                    }
                    TranscriptSection(chat: chat)
                }
                .listStyle(.insetGrouped)
                .refreshable { await chat.refresh() }
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
            Text(seat.summary).font(.caption).foregroundStyle(.secondary)
            Text("Last activity \(shortTime(seat.lastActivityAt))")
                .font(.caption2).foregroundStyle(.secondary)
        }
    }
}

struct PendingBadge: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.caption.bold())
            .padding(.horizontal, 8).padding(.vertical, 2)
            .background(.orange, in: Capsule())
            .foregroundStyle(.white)
            .accessibilityLabel("\(text) pending decisions")
    }
}

struct ComposerView: View {
    @Bindable var chat: ManagerChat

    var body: some View {
        let draft = chat.draft
        VStack(alignment: .leading, spacing: 8) {
            if chat.messagingOff {
                MessagingOffNotice()
            }
            TextField("Message the manager", text: $chat.draft.text, axis: .vertical)
                .lineLimit(1...6)
                .disabled(draft.locked || draft.sending || chat.composeDisabled)
            HStack {
                Text("\(draft.text.utf8.count)/\(maxMessageBytes) bytes")
                    .font(.caption2)
                    .foregroundStyle(draft.text.utf8.count > maxMessageBytes ? .red : .secondary)
                Spacer()
                if draft.locked {
                    Button("Discard", role: .destructive) { chat.discard() }
                        .disabled(draft.sending)
                    Button("Retry same message") { Task { await chat.send() } }
                        .buttonStyle(.borderedProminent)
                        .disabled(draft.sending)
                } else {
                    Button("Send") { Task { await chat.send() } }
                        .buttonStyle(.borderedProminent)
                        .disabled(draft.sending || chat.composeDisabled || messageProblem(draft.text) != nil)
                }
            }
            .buttonStyle(.bordered)
            status(draft)
        }
        .padding(.vertical, 4)
    }

    @ViewBuilder private func status(_ draft: MessageDraft) -> some View {
        switch draft.phase {
        case .idle:
            EmptyView()
        case .sending:
            HStack(spacing: 6) {
                ProgressView()
                Text("Sending…").font(.caption).foregroundStyle(.secondary)
            }
        case .sent(let receipt):
            let delivery = chat.delivery(of: receipt)
            Text(delivery.text).font(.caption).foregroundStyle(delivery.tone.color)
        case .failed(let error) where error.kind == .messagingOff:
            // The notice above says it calmly; this is not an error.
            EmptyView()
        case .failed(let error):
            Text(error.message).font(.caption)
                .foregroundStyle(error.kind == .unknown ? .orange : .red)
        }
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

struct TranscriptSection: View {
    @Bindable var chat: ManagerChat

    var body: some View {
        let thread = chat.thread
        Section {
            if let thread, let error = thread.error {
                Label(error.message, systemImage: error.isUnreachable ? "wifi.exclamationmark" : "exclamationmark.triangle")
                    .font(.caption).foregroundStyle(.red)
            }
            if let thread, thread.loaded {
                if thread.items.isEmpty {
                    Text("No messages yet.").foregroundStyle(.secondary)
                }
                ForEach(thread.items) { item in
                    ConversationRow(
                        item: item,
                        delivery: item.operatorMessageId.flatMap { id in
                            thread.operatorMessages.first { $0.messageId == id }?.state
                        })
                }
                if thread.hasMore {
                    Button {
                        Task { await chat.loadOlder() }
                    } label: {
                        HStack {
                            Text("Load older")
                            if thread.loadingOlder { Spacer(); ProgressView() }
                        }
                    }
                    .disabled(thread.loadingOlder)
                }
                if let error = thread.olderError {
                    Text(error.message).font(.caption).foregroundStyle(.red)
                }
            } else if thread?.error == nil {
                ProgressView("Loading conversation…")
            }
        } header: {
            Text("Conversation, newest first")
        }
    }
}

struct ConversationRow: View {
    let item: ConversationItem
    let delivery: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(item.speakerLabel)
                    .font(.caption.bold())
                    .foregroundStyle(item.speaker == "operator" ? Color.accentColor : .primary)
                Spacer()
                Text(shortTime(item.createdAt)).font(.caption2).foregroundStyle(.secondary)
            }
            Text(item.text)
                .font(item.speaker == "system" ? .callout.italic() : .callout)
                .foregroundStyle(item.speaker == "system" ? .secondary : .primary)
                .textSelection(.enabled)
            if item.truncated {
                Text("Cut at 4096 bytes; the full text is on the hub.")
                    .font(.caption2).foregroundStyle(.secondary)
            }
            if let delivery {
                let shown = describeDelivery(delivery)
                Text(shown.text).font(.caption2).foregroundStyle(shown.tone.color)
            }
        }
        .padding(.vertical, 2)
    }
}

extension AnswerTone {
    var color: Color {
        switch self {
        case .idle, .busy: .secondary
        case .ok: .green
        case .warn: .orange
        case .error: .red
        }
    }
}

/// `2026-10-17T09:24:00.000000000Z` → `10-17 09:24 UTC`; other text unchanged.
func shortTime(_ rfc3339: String) -> String {
    let chars = Array(rfc3339)
    guard chars.count >= 17, chars[4] == "-", chars[10] == "T", rfc3339.hasSuffix("Z") else {
        return rfc3339
    }
    return String(chars[5..<10]) + " " + String(chars[11..<16]) + " UTC"
}
