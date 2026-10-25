import Foundation

// The block parser behind every message the app shows (#503, replacing the
// inline-only text of #159). Pure and total: any text in, a document out, with
// unsupported or malformed syntax kept as plain paragraph text. No third-party
// dependency; inline syntax (bold, italics, code, links) stays inside block
// text and is rendered by `inlineMarkdown` / `styledInline`.

/// Column alignment of a table, from the `:---:` row.
enum MarkdownAlign: Equatable, Sendable { case leading, center, trailing }

struct MarkdownTable: Equatable, Sendable {
    var header: [String]
    var aligns: [MarkdownAlign]
    var rows: [[String]]
}

struct MarkdownListItem: Equatable, Sendable {
    var blocks: [MarkdownBlock]
    /// A task-list item (`[ ]` / `[x]`): its state.
    var checked: Bool?
}

indirect enum MarkdownBlock: Equatable, Sendable {
    /// Inline Markdown with its line breaks kept.
    case paragraph(String)
    case heading(level: Int, text: String)
    case list(ordered: Bool, start: Int, items: [MarkdownListItem])
    case quote([MarkdownBlock])
    /// A fenced code block; `language` is the first word of the info string.
    case code(language: String?, text: String)
    case table(MarkdownTable)
    /// An image the text names (`![alt](url)` or a bare image URL on its own line);
    /// whether and when it loads is `classifyImageSource`'s and the view's business.
    case image(alt: String, url: String)
    case rule
}

struct MarkdownDocument: Equatable, Sendable {
    var blocks: [MarkdownBlock]
    /// Characters past the input cap that were not parsed (0 for any normal message).
    var omittedCharacters = 0
}

/// The bounds that keep a hostile or enormous message from costing more than a
/// bounded amount of work and layout. The hub caps one event at 4096 bytes, so a
/// normal message is far below all of them.
enum MarkdownLimits {
    static let maxCharacters = 65_536
    static let maxLines = 4_000
    static let maxBlocks = 2_000
    static let maxDepth = 6
    static let maxTableColumns = 24
    static let maxTableRows = 200
    static let maxImagesPerMessage = 12
}

/// Parses Markdown text. Never fails and never traps: whatever the parser does not
/// understand is paragraph text.
func parseMarkdown(_ text: String) -> MarkdownDocument {
    var source = Substring(text)
    var omitted = 0
    if text.utf8.count > MarkdownLimits.maxCharacters * 4 || text.count > MarkdownLimits.maxCharacters {
        let kept = text.prefix(MarkdownLimits.maxCharacters)
        omitted = text.count - kept.count
        source = kept
    }
    var lines = source.replacingOccurrences(of: "\r\n", with: "\n")
        .replacingOccurrences(of: "\r", with: "\n")
        .components(separatedBy: "\n")
    if lines.count > MarkdownLimits.maxLines {
        omitted += lines[MarkdownLimits.maxLines...].reduce(0) { $0 + $1.count + 1 }
        lines = Array(lines[..<MarkdownLimits.maxLines])
    }
    var parser = MarkdownParser()
    let blocks = parser.parse(lines.map(expandTabs), depth: 0)
    return MarkdownDocument(blocks: blocks, omittedCharacters: omitted)
}

private func expandTabs(_ line: String) -> String {
    guard line.contains("\t") else { return line }
    var out = ""
    var leading = true
    for c in line {
        if c == "\t" {
            out += leading ? "    " : " "
        } else {
            if c != " " { leading = false }
            out.append(c)
        }
    }
    return out
}

// MARK: - Line classification

private func indentWidth(_ line: String) -> Int { line.prefix { $0 == " " }.count }

private func isBlank(_ line: String) -> Bool { line.allSatisfy { $0 == " " } }

private struct Fence {
    let marker: Character
    let length: Int
    let language: String?
}

private func fenceOpen(_ line: String) -> Fence? {
    guard indentWidth(line) <= 3 else { return nil }
    let trimmed = line.drop { $0 == " " }
    guard let marker = trimmed.first, marker == "`" || marker == "~" else { return nil }
    let run = trimmed.prefix { $0 == marker }.count
    guard run >= 3 else { return nil }
    let info = trimmed.dropFirst(run).trimmingCharacters(in: .whitespaces)
    if marker == "`" && info.contains("`") { return nil }
    let language = info.split(whereSeparator: { $0 == " " || $0 == "{" }).first.map(String.init)
    return Fence(marker: marker, length: run, language: language?.isEmpty == false ? language : nil)
}

private func closesFence(_ line: String, _ fence: Fence) -> Bool {
    guard indentWidth(line) <= 3 else { return false }
    let trimmed = line.trimmingCharacters(in: .whitespaces)
    return trimmed.count >= fence.length && trimmed.allSatisfy { $0 == fence.marker }
}

private func atxHeading(_ line: String) -> MarkdownBlock? {
    guard indentWidth(line) <= 3 else { return nil }
    let trimmed = line.trimmingCharacters(in: .whitespaces)
    let hashes = trimmed.prefix { $0 == "#" }.count
    guard (1...6).contains(hashes) else { return nil }
    let rest = trimmed.dropFirst(hashes)
    guard rest.isEmpty || rest.first == " " else { return nil }
    var title = rest.trimmingCharacters(in: .whitespaces)
    while title.hasSuffix("#") { title.removeLast() }
    return .heading(level: hashes, text: title.trimmingCharacters(in: .whitespaces))
}

private func isRule(_ line: String) -> Bool {
    guard indentWidth(line) <= 3 else { return false }
    let compact = line.filter { $0 != " " }
    guard compact.count >= 3, let first = compact.first, "-*_".contains(first) else { return false }
    return compact.allSatisfy { $0 == first }
}

private func isQuote(_ line: String) -> Bool {
    indentWidth(line) <= 3 && line.drop { $0 == " " }.hasPrefix(">")
}

private func unquote(_ line: String) -> String {
    var rest = line.drop { $0 == " " }.dropFirst()
    if rest.first == " " { rest = rest.dropFirst() }
    return String(rest)
}

private struct ListMarker {
    let indent: Int
    let ordered: Bool
    let number: Int
    /// The column where the item's content starts.
    let contentOffset: Int
    let content: String
}

private func listMarker(_ line: String) -> ListMarker? {
    let indent = indentWidth(line)
    let body = line.dropFirst(indent)
    guard let first = body.first else { return nil }
    if "-*+".contains(first) {
        let after = body.dropFirst()
        guard after.isEmpty || after.first == " " else { return nil }
        return ListMarker(
            indent: indent, ordered: false, number: 0, contentOffset: indent + 2,
            content: String(after.dropFirst()))
    }
    let digits = body.prefix { $0.isASCII && $0.isNumber }
    guard !digits.isEmpty, digits.count <= 9 else { return nil }
    let after = body.dropFirst(digits.count)
    guard let punct = after.first, punct == "." || punct == ")" else { return nil }
    let rest = after.dropFirst()
    guard rest.isEmpty || rest.first == " " else { return nil }
    return ListMarker(
        indent: indent, ordered: true, number: Int(digits) ?? 1,
        contentOffset: indent + digits.count + 2, content: String(rest.dropFirst()))
}

/// Splits a table row on its unescaped pipes outside code spans.
private func tableCells(_ line: String) -> [String] {
    var cells: [String] = []
    var current = ""
    var inCode = false
    var escaped = false
    for c in line.trimmingCharacters(in: .whitespaces) {
        if escaped {
            if c != "|" { current.append("\\") }
            current.append(c)
            escaped = false
        } else if c == "\\" {
            escaped = true
        } else if c == "`" {
            inCode.toggle()
            current.append(c)
        } else if c == "|" && !inCode {
            cells.append(current)
            current = ""
        } else {
            current.append(c)
        }
    }
    if escaped { current.append("\\") }
    cells.append(current)
    if line.trimmingCharacters(in: .whitespaces).hasPrefix("|"), !cells.isEmpty { cells.removeFirst() }
    if line.trimmingCharacters(in: .whitespaces).hasSuffix("|"), cells.count > 0,
        !line.trimmingCharacters(in: .whitespaces).hasSuffix("\\|")
    {
        cells.removeLast()
    }
    return cells.map { $0.trimmingCharacters(in: .whitespaces) }
}

private func separatorAligns(_ line: String) -> [MarkdownAlign]? {
    guard line.contains("-"), line.contains("|") else { return nil }
    let cells = tableCells(line)
    guard !cells.isEmpty else { return nil }
    var aligns: [MarkdownAlign] = []
    for cell in cells {
        let left = cell.hasPrefix(":")
        let right = cell.hasSuffix(":")
        let dashes = cell.dropFirst(left ? 1 : 0).dropLast(right ? 1 : 0)
        guard !dashes.isEmpty, dashes.allSatisfy({ $0 == "-" }) else { return nil }
        aligns.append(left && right ? .center : right ? .trailing : .leading)
    }
    return aligns
}

// MARK: - Block parser

private struct MarkdownParser {
    var blockBudget = MarkdownLimits.maxBlocks
    var imageBudget = MarkdownLimits.maxImagesPerMessage

    mutating func parse(_ lines: [String], depth: Int) -> [MarkdownBlock] {
        if depth > MarkdownLimits.maxDepth {
            // Nested deeper than we draw: keep it readable as text.
            let text = lines.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines)
            return text.isEmpty ? [] : [.paragraph(text)]
        }
        var blocks: [MarkdownBlock] = []
        var paragraph: [String] = []

        func flush(_ parser: inout MarkdownParser) {
            guard !paragraph.isEmpty else { return }
            blocks.append(contentsOf: parser.paragraphBlocks(paragraph))
            paragraph = []
        }

        var i = 0
        let n = lines.count
        while i < n {
            if blockBudget <= 0 { break }
            let line = lines[i]
            if isBlank(line) {
                flush(&self)
                i += 1
                continue
            }
            if let fence = fenceOpen(line) {
                flush(&self)
                var body: [String] = []
                i += 1
                while i < n, !closesFence(lines[i], fence) {
                    body.append(lines[i])
                    i += 1
                }
                i += 1  // the closing fence; an unclosed fence runs to the end
                blocks.append(.code(language: fence.language, text: body.joined(separator: "\n")))
                blockBudget -= 1
                continue
            }
            if let heading = atxHeading(line) {
                flush(&self)
                blocks.append(heading)
                blockBudget -= 1
                i += 1
                continue
            }
            if isRule(line) {
                flush(&self)
                blocks.append(.rule)
                blockBudget -= 1
                i += 1
                continue
            }
            if line.contains("|"), i + 1 < n, let table = parseTable(lines, at: i) {
                flush(&self)
                blocks.append(.table(table.table))
                blockBudget -= 1
                i = table.next
                continue
            }
            if isQuote(line) {
                flush(&self)
                var inner: [String] = []
                while i < n {
                    let l = lines[i]
                    if isQuote(l) {
                        inner.append(unquote(l))
                    } else if !isBlank(l), !inner.isEmpty, !isBlank(inner.last ?? ""), !startsBlock(l) {
                        inner.append(l)  // lazy continuation
                    } else {
                        break
                    }
                    i += 1
                }
                let content = parse(inner, depth: depth + 1)
                blocks.append(.quote(content))
                blockBudget -= 1
                continue
            }
            if let marker = listMarker(line) {
                flush(&self)
                let list = parseList(lines, from: i, first: marker, depth: depth)
                blocks.append(list.block)
                blockBudget -= 1
                i = list.next
                continue
            }
            paragraph.append(line)
            i += 1
        }
        flush(&self)
        return blocks
    }

    /// Lines that start a block of their own and so end a paragraph or a lazy line.
    private func startsBlock(_ line: String) -> Bool {
        fenceOpen(line) != nil || atxHeading(line) != nil || isRule(line) || isQuote(line)
            || listMarker(line) != nil
    }

    private func parseTable(_ lines: [String], at i: Int) -> (table: MarkdownTable, next: Int)? {
        let header = tableCells(lines[i])
        guard header.count >= 1, header.count <= MarkdownLimits.maxTableColumns,
            let aligns = separatorAligns(lines[i + 1]), aligns.count == header.count,
            lines[i].contains("|")
        else { return nil }
        var rows: [[String]] = []
        var next = i + 2
        var dropped = 0
        while next < lines.count, !isBlank(lines[next]), lines[next].contains("|"),
            fenceOpen(lines[next]) == nil, atxHeading(lines[next]) == nil, !isQuote(lines[next])
        {
            if rows.count < MarkdownLimits.maxTableRows {
                var cells = tableCells(lines[next])
                if cells.count > header.count { cells = Array(cells[..<header.count]) }
                while cells.count < header.count { cells.append("") }
                rows.append(cells)
            } else {
                dropped += 1
            }
            next += 1
        }
        if dropped > 0 {
            rows.append(["… \(dropped) more rows"] + Array(repeating: "", count: header.count - 1))
        }
        return (MarkdownTable(header: header, aligns: aligns, rows: rows), next)
    }

    private mutating func parseList(
        _ lines: [String], from start: Int, first: ListMarker, depth: Int
    ) -> (block: MarkdownBlock, next: Int) {
        let n = lines.count
        let base = first.indent
        var items: [MarkdownListItem] = []
        var i = start
        while i < n, let marker = listMarker(lines[i]), marker.ordered == first.ordered,
            marker.indent < base + 2, items.count < 500
        {
            var body = [marker.content]
            i += 1
            itemLoop: while i < n {
                let l = lines[i]
                if isBlank(l) {
                    // Blank lines stay in the item only when an indented line follows.
                    var j = i
                    while j < n, isBlank(lines[j]) { j += 1 }
                    if j < n, indentWidth(lines[j]) >= base + 2 {
                        body.append(contentsOf: Array(repeating: "", count: j - i))
                        i = j
                        continue
                    }
                    // A sibling after a blank line keeps the list going.
                    if j < n, let m = listMarker(lines[j]), m.ordered == first.ordered, m.indent < base + 2 {
                        i = j
                    }
                    break itemLoop
                }
                if indentWidth(l) >= base + 2 {
                    let strip = min(indentWidth(l), marker.contentOffset)
                    body.append(String(l.dropFirst(strip)))
                    i += 1
                    continue
                }
                if listMarker(l) != nil || startsBlock(l) { break itemLoop }
                if let last = body.last, !isBlank(last) {
                    body.append(l.trimmingCharacters(in: .whitespaces))  // lazy continuation
                    i += 1
                    continue
                }
                break itemLoop
            }
            var checked: Bool?
            if let head = body.first {
                if head.hasPrefix("[ ] ") || head == "[ ]" {
                    checked = false
                    body[0] = String(head.dropFirst(min(4, head.count)))
                } else if head.hasPrefix("[x] ") || head.hasPrefix("[X] ") || head == "[x]" || head == "[X]" {
                    checked = true
                    body[0] = String(head.dropFirst(min(4, head.count)))
                }
            }
            items.append(MarkdownListItem(blocks: parse(body, depth: depth + 1), checked: checked))
        }
        return (.list(ordered: first.ordered, start: first.number, items: items), i)
    }

    // MARK: Paragraphs and images

    private mutating func paragraphBlocks(_ lines: [String]) -> [MarkdownBlock] {
        var out: [MarkdownBlock] = []
        var buffer: [String] = []
        func emitText(_ parser: inout MarkdownParser) {
            guard !buffer.isEmpty else { return }
            out.append(contentsOf: parser.splitInlineImages(buffer.joined(separator: "\n")))
            buffer = []
        }
        for line in lines {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if imageBudget > 0, isBareImageURL(trimmed) {
                emitText(&self)
                imageBudget -= 1
                out.append(.image(alt: "", url: trimmed))
            } else {
                buffer.append(line)
            }
        }
        emitText(&self)
        return out
    }

    /// Cuts `![alt](url)` out of paragraph text, leaving the text on either side as
    /// paragraphs. Code spans are left alone; a malformed image stays text.
    private mutating func splitInlineImages(_ text: String) -> [MarkdownBlock] {
        guard text.contains("![") else { return textBlock(text) }
        let chars = Array(text)
        var out: [MarkdownBlock] = []
        var pending = ""
        var i = 0
        func flush(_ parser: inout MarkdownParser) {
            out.append(contentsOf: parser.textBlock(pending))
            pending = ""
        }
        while i < chars.count {
            let c = chars[i]
            if c == "`" {
                var run = 0
                while i + run < chars.count, chars[i + run] == "`" { run += 1 }
                var j = i + run
                var closed: Int?
                while j < chars.count {
                    if chars[j] == "`" {
                        var r = 0
                        while j + r < chars.count, chars[j + r] == "`" { r += 1 }
                        if r == run {
                            closed = j + r
                            break
                        }
                        j += r
                    } else {
                        j += 1
                    }
                }
                let end = closed ?? (i + run)
                pending.append(contentsOf: chars[i..<end])
                i = end
                continue
            }
            if c == "\\", i + 1 < chars.count {
                pending.append(c)
                pending.append(chars[i + 1])
                i += 2
                continue
            }
            if imageBudget > 0 {
                // [![alt](img)](href): a linked image is the image.
                if c == "[", i + 2 < chars.count, chars[i + 1] == "!", chars[i + 2] == "[",
                    let image = parseImage(chars, at: i + 1), image.end < chars.count - 1,
                    chars[image.end] == "]", chars[image.end + 1] == "(",
                    let close = closingParen(chars, from: image.end + 2)
                {
                    flush(&self)
                    imageBudget -= 1
                    out.append(.image(alt: image.alt, url: image.url))
                    i = close + 1
                    continue
                }
                if c == "!", i + 1 < chars.count, chars[i + 1] == "[",
                    let image = parseImage(chars, at: i)
                {
                    flush(&self)
                    imageBudget -= 1
                    out.append(.image(alt: image.alt, url: image.url))
                    i = image.end
                    continue
                }
            }
            pending.append(c)
            i += 1
        }
        flush(&self)
        return out
    }

    private func textBlock(_ text: String) -> [MarkdownBlock] {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? [] : [.paragraph(trimmed)]
    }
}

/// The index of the `)` closing the one opened just before `from`, balancing
/// nested parentheses and not crossing a line break.
private func closingParen(_ chars: [Character], from: Int) -> Int? {
    var depth = 1
    var j = from
    while j < chars.count {
        let c = chars[j]
        if c == "\n" { return nil }
        if c == "(" { depth += 1 }
        if c == ")" {
            depth -= 1
            if depth == 0 { return j }
        }
        j += 1
    }
    return nil
}

/// `![alt](url "title")` at `chars[at]` (the `!`); `end` is the index after the `)`.
private func parseImage(_ chars: [Character], at: Int) -> (alt: String, url: String, end: Int)? {
    guard at + 1 < chars.count, chars[at] == "!", chars[at + 1] == "[" else { return nil }
    var j = at + 2
    var depth = 1
    var alt = ""
    while j < chars.count {
        let c = chars[j]
        if c == "\n" { return nil }
        if c == "\\", j + 1 < chars.count {
            alt.append(chars[j + 1])
            j += 2
            continue
        }
        if c == "[" { depth += 1 }
        if c == "]" {
            depth -= 1
            if depth == 0 { break }
        }
        alt.append(c)
        j += 1
    }
    guard j < chars.count, depth == 0, j + 1 < chars.count, chars[j + 1] == "(" else { return nil }
    guard let close = closingParen(chars, from: j + 2) else { return nil }
    var inner = String(chars[(j + 2)..<close]).trimmingCharacters(in: .whitespaces)
    if inner.hasPrefix("<"), let end = inner.firstIndex(of: ">") {
        inner = String(inner[inner.index(after: inner.startIndex)..<end])
    } else if let space = inner.firstIndex(where: { $0 == " " }) {
        inner = String(inner[..<space])  // the title after the url
    }
    guard !inner.isEmpty else { return nil }
    return (alt.trimmingCharacters(in: .whitespaces), inner, close + 1)
}

/// A line that is only an http(s) URL ending in an image extension.
func isBareImageURL(_ trimmed: String) -> Bool {
    guard trimmed.count <= 2048, !trimmed.contains(" "),
        trimmed.lowercased().hasPrefix("https://") || trimmed.lowercased().hasPrefix("http://"),
        let url = URL(string: trimmed)
    else { return false }
    return ["png", "jpg", "jpeg", "gif", "webp", "heic"].contains(url.pathExtension.lowercased())
}
