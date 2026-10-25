import Foundation

// Inline Markdown for the block views (#503): Apple's parser for bold, italics,
// strikethrough, code and links, then two safety passes of ours. A link the
// reader could tap is `http`, `https` or `mailto` only (no relative links, no
// `javascript:`, `tel:`, or another app's scheme); bare `https://` URLs become
// links so a pasted address is tappable like in the chat apps.

/// Whether a link from message text may be opened by a tap.
func isSafeLink(_ url: URL) -> Bool {
    switch url.scheme?.lowercased() {
    case "http", "https": return !(url.host() ?? "").isEmpty
    case "mailto": return true
    default: return false
    }
}

/// Inline text ready for `Text`: unsafe links removed, `<br>` as a line break and
/// bare web addresses linked. Total: unparsable text comes back as plain text.
func sanitizedInline(_ text: String) -> AttributedString {
    var source = text
    if source.contains("<") {
        for br in ["<br>", "<br/>", "<br />", "<BR>"] {
            source = source.replacingOccurrences(of: br, with: "\n")
        }
    }
    var out = inlineMarkdown(source)
    var unsafe: [Range<AttributedString.Index>] = []
    for run in out.runs {
        if let url = run.link, !isSafeLink(url) { unsafe.append(run.range) }
    }
    for range in unsafe { out[range].link = nil }
    linkBareAddresses(&out)
    return out
}

private func linkBareAddresses(_ text: inout AttributedString) {
    let plain = String(text.characters)
    guard plain.contains("http://") || plain.contains("https://"),
        plain.count <= 20_000,
        let detector = try? NSDataDetector(types: NSTextCheckingResult.CheckingType.link.rawValue)
    else { return }
    var targets: [(Range<AttributedString.Index>, URL)] = []
    for match in detector.matches(in: plain, range: NSRange(plain.startIndex..., in: plain)) {
        guard let range = Range(match.range, in: plain), let url = match.url, isSafeLink(url),
            plain[range].lowercased().hasPrefix("http")
        else { continue }
        let lower = text.characters.index(
            text.startIndex, offsetBy: plain.distance(from: plain.startIndex, to: range.lowerBound))
        let upper = text.characters.index(
            text.startIndex, offsetBy: plain.distance(from: plain.startIndex, to: range.upperBound))
        let slice = text[lower..<upper]
        let taken = slice.runs.contains { run in
            run.link != nil || run.inlinePresentationIntent?.contains(.code) == true
        }
        if !taken { targets.append((lower..<upper, url)) }
    }
    for (range, url) in targets { text[range].link = url }
}
