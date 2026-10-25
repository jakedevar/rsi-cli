import SwiftUI
import UIKit

extension UIPasteboard: TextPasteboard {
    func write(_ text: String) { string = text }
}

/// A fenced code block: a language label and a copy button over horizontally
/// scrolling, syntax-highlighted monospaced text. A very long block shows its
/// first lines and a button for the rest; copying always copies every line.
struct CodeBlockView: View {
    @Environment(\.theme) private var theme
    let language: String?
    let code: String
    @State private var copied = false
    @State private var expanded = false

    nonisolated static let foldAbove = 200
    nonisolated static let previewLines = 120

    private var lines: [Substring] { code.split(separator: "\n", omittingEmptySubsequences: false) }

    var body: some View {
        let all = lines
        let folded = all.count > Self.foldAbove && !expanded
        let shown = folded ? all.prefix(Self.previewLines).joined(separator: "\n") : code
        VStack(alignment: .leading, spacing: 0) {
            header
            ScrollView(.horizontal, showsIndicators: false) {
                Text(highlighted(shown))
                    .font(.system(.caption, design: .monospaced))
                    .fixedSize(horizontal: true, vertical: true)
                    .textSelection(.enabled)
                    .padding(.horizontal, 10).padding(.bottom, 10).padding(.top, 2)
            }
            if folded {
                Button {
                    withAnimation(.snappy(duration: 0.2)) { expanded = true }
                } label: {
                    Text("Show \(all.count - Self.previewLines) more lines")
                        .font(.caption.weight(.semibold))
                        .frame(maxWidth: .infinity, minHeight: 36)
                }
                .buttonStyle(.plain)
                .foregroundStyle(theme.mdLink.color)
            }
        }
        .background(theme.codeBg.color, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(theme.border.color.opacity(0.35), lineWidth: 0.5))
        .accessibilityElement(children: .contain)
    }

    private var header: some View {
        HStack(spacing: 6) {
            Text(language ?? "code")
                .font(.caption2.weight(.semibold)).foregroundStyle(theme.mutedText.color)
                .textCase(.lowercase)
            Spacer(minLength: 8)
            Button {
                copy()
            } label: {
                HStack(spacing: 4) {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    Text(copied ? "Copied" : "Copy")
                }
                .font(.caption2.weight(.semibold))
                .foregroundStyle((copied ? theme.running : theme.mutedText).color)
                .padding(.horizontal, 8)
                .frame(minHeight: 32)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(copied ? "Copied" : "Copy code")
        }
        .padding(.leading, 10).padding(.trailing, 2)
    }

    private func copy() {
        CodeCopy.copy(code, to: UIPasteboard.general)
        UINotificationFeedbackGenerator().notificationOccurred(.success)
        copied = true
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(1.6))
            copied = false
        }
    }

    private func highlighted(_ shown: String) -> AttributedString {
        var out = AttributedString()
        for span in SyntaxCache.shared.spans(shown, language: language) {
            var piece = AttributedString(span.text)
            piece.foregroundColor = theme.syntaxColor(span.kind).color
            out.append(piece)
        }
        if out.characters.isEmpty {
            var empty = AttributedString(" ")
            empty.foregroundColor = theme.codeFg.color
            out = empty
        }
        return out
    }
}

/// A table: a header row on the panel tier and bordered cells, aligned as the
/// separator row says; wide tables scroll sideways.
struct MarkdownTableView: View {
    @Environment(\.theme) private var theme
    let table: MarkdownTable
    let font: Font

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            Grid(alignment: .topLeading, horizontalSpacing: 0, verticalSpacing: 0) {
                GridRow {
                    ForEach(Array(table.header.enumerated()), id: \.offset) { column, cell in
                        cellView(cell, column: column, header: true)
                    }
                }
                ForEach(Array(table.rows.enumerated()), id: \.offset) { _, row in
                    GridRow {
                        ForEach(Array(row.enumerated()), id: \.offset) { column, cell in
                            cellView(cell, column: column, header: false)
                        }
                    }
                }
            }
            .overlay(Rectangle().strokeBorder(theme.border.color.opacity(0.5), lineWidth: 0.5))
        }
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }

    private func alignment(_ column: Int) -> Alignment {
        guard column < table.aligns.count else { return .leading }
        switch table.aligns[column] {
        case .leading: return .leading
        case .center: return .center
        case .trailing: return .trailing
        }
    }

    private func cellView(_ text: String, column: Int, header: Bool) -> some View {
        Text(themedInline(text, theme: theme))
            .font(header ? font.bold() : font)
            .multilineTextAlignment(
                table.aligns.indices.contains(column)
                    ? (table.aligns[column] == .trailing ? .trailing : table.aligns[column] == .center ? .center : .leading)
                    : .leading
            )
            .frame(minWidth: 56, maxWidth: 240, alignment: alignment(column))
            .padding(.horizontal, 10).padding(.vertical, 6)
            .background(header ? theme.panel.color : Color.clear)
            .overlay(Rectangle().strokeBorder(theme.border.color.opacity(0.3), lineWidth: 0.5))
    }
}
