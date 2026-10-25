import SwiftUI

/// Message text rendered as Markdown, like the chat apps (#503): headings,
/// nested lists, task lists, tables, block quotes, rules, fenced code with syntax
/// highlighting and a copy button, links, and images that load on request. The
/// parser (Model/Markdown.swift) is total, so a message that is not Markdown, or
/// is malformed, reads as plain text. Used by every message surface.
struct MarkdownText: View {
    let text: String
    var font: Font = .body

    var body: some View {
        let document = MarkdownCache.shared.document(text)
        VStack(alignment: .leading, spacing: 8) {
            MarkdownBlocks(blocks: document.blocks, font: font)
            if document.omittedCharacters > 0 {
                Text("… \(document.omittedCharacters) more characters not shown")
                    .font(.caption2).foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A run of blocks, recursive for lists and quotes.
struct MarkdownBlocks: View {
    @Environment(\.theme) private var theme
    let blocks: [MarkdownBlock]
    let font: Font
    var listDepth = 0
    var spacing: CGFloat = 8

    var body: some View {
        VStack(alignment: .leading, spacing: spacing) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                blockView(block)
            }
        }
    }

    @ViewBuilder
    private func blockView(_ block: MarkdownBlock) -> some View {
        switch block {
        case .paragraph(let text):
            Text(themedInline(text, theme: theme)).font(font)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .heading(let level, let text):
            Text(themedInline(text, theme: theme)).font(Self.headingFont(level))
                .foregroundStyle((level <= 2 ? theme.mdHeader : theme.mdHeaderMinor).color)
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityAddTraits(.isHeader)
        case .list(let ordered, let start, let items):
            MarkdownListView(ordered: ordered, start: start, items: items, font: font, depth: listDepth)
        case .quote(let inner):
            HStack(alignment: .top, spacing: 8) {
                Rectangle().fill(theme.mdBlockquote.color.opacity(0.6)).frame(width: 3)
                MarkdownBlocks(blocks: inner, font: font, listDepth: listDepth, spacing: 6)
                    .foregroundStyle(theme.mdBlockquote.color)
            }
            .fixedSize(horizontal: false, vertical: true)
        case .code(let language, let text):
            CodeBlockView(language: language, code: text)
        case .table(let table):
            MarkdownTableView(table: table, font: font)
        case .image(let alt, let url):
            MarkdownImageView(alt: alt, source: url)
        case .rule:
            Rectangle().fill(theme.mdRule.color).frame(height: 1).padding(.vertical, 4)
        }
    }

    static func headingFont(_ level: Int) -> Font {
        switch level {
        case 1: .title2.bold()
        case 2: .title3.bold()
        case 3: .headline
        default: .subheadline.bold()
        }
    }
}

struct MarkdownListView: View {
    @Environment(\.theme) private var theme
    let ordered: Bool
    let start: Int
    let items: [MarkdownListItem]
    let font: Font
    let depth: Int

    private static let bullets = ["•", "◦", "–"]

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(Array(items.enumerated()), id: \.offset) { index, item in
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    marker(index, item)
                    MarkdownBlocks(blocks: item.blocks, font: font, listDepth: depth + 1, spacing: 4)
                }
            }
        }
        .padding(.leading, depth > 0 ? 2 : 0)
    }

    @ViewBuilder
    private func marker(_ index: Int, _ item: MarkdownListItem) -> some View {
        if let checked = item.checked {
            Image(systemName: checked ? "checkmark.square.fill" : "square")
                .font(font).foregroundStyle(theme.mdListBullet.color)
                .frame(minWidth: 16)
                .accessibilityLabel(checked ? "Done" : "Not done")
        } else if ordered {
            Text("\(start + index).").font(font).monospacedDigit()
                .foregroundStyle(theme.mdListBullet.color)
                .frame(minWidth: 22, alignment: .trailing)
        } else {
            Text(Self.bullets[depth % Self.bullets.count]).font(font)
                .foregroundStyle(theme.mdListBullet.color)
                .frame(minWidth: 16)
        }
    }
}

/// Inline Markdown in the active theme: inline code on the code background, and
/// the link safety of `sanitizedInline`.
func themedInline(_ text: String, theme: RemoteTheme) -> AttributedString {
    var out = sanitizedInline(text)
    var code: [Range<AttributedString.Index>] = []
    for run in out.runs where run.inlinePresentationIntent?.contains(.code) == true {
        code.append(run.range)
    }
    for range in code {
        out[range].backgroundColor = theme.codeBg.color
        out[range].foregroundColor = theme.codeFg.color
    }
    return out
}
