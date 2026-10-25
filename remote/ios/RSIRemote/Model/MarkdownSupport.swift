import Foundation

// Small pure helpers behind the Markdown views (#503): the syntax colours with
// their contrast guarantee, the copy action, and the document cache.

extension RGB {
    /// Mixed toward white by `amount` (0 unchanged, 1 white).
    func lightened(by amount: Double) -> RGB {
        func channel(_ value: UInt8) -> UInt8 { UInt8((Double(value) + (255 - Double(value)) * amount).rounded()) }
        return RGB(channel(r), channel(g), channel(b))
    }

    /// This colour, moved toward black (on a light background) or white (on a dark
    /// one) until it reads on `background` at `minimum` contrast.
    func ensuringContrast(on background: RGB, minimum: Double = 4.0) -> RGB {
        guard contrast(with: background) < minimum else { return self }
        let towardBlack = background.luminance > 0.4
        var step = 0.05
        while step <= 1.0 {
            let moved = towardBlack ? darkened(by: step) : lightened(by: step)
            if moved.contrast(with: background) >= minimum { return moved }
            step += 0.05
        }
        return towardBlack ? RGB(0, 0, 0) : RGB(255, 255, 255)
    }
}

extension RemoteTheme {
    /// The colour of a token kind in a code block: the palette's accent for it,
    /// moved only as far as needed to read on the code background in this theme
    /// (dark or its light variant).
    func syntaxColor(_ kind: SyntaxKind) -> RGB {
        let base: RGB =
            switch kind {
            case .plain: codeFg
            case .keyword: palette.mauve
            case .string: palette.green
            case .comment: palette.overlay1
            case .number: palette.peach
            case .type: palette.yellow
            case .function: palette.blue
            case .added: palette.green
            case .removed: palette.red
            case .meta: palette.sapphire
            }
        return base.ensuringContrast(on: codeBg)
    }
}

/// Where copied code goes; `UIPasteboard` in the app, a recorder in tests.
protocol TextPasteboard {
    func write(_ text: String)
}

enum CodeCopy {
    /// Puts exactly `code` on the pasteboard (what the block holds, not what it
    /// shows: a folded block still copies every line). Returns whether it did.
    @discardableResult
    static func copy(_ code: String, to pasteboard: TextPasteboard) -> Bool {
        pasteboard.write(code)
        return true
    }
}

/// Parsed documents by message text. The conversation redraws rows while it
/// scrolls; a message is parsed once.
final class MarkdownCache: @unchecked Sendable {
    static let shared = MarkdownCache()
    private final class Box {
        let document: MarkdownDocument
        init(_ document: MarkdownDocument) { self.document = document }
    }
    private let cache = NSCache<NSString, Box>()

    init(limit: Int = 300) { cache.countLimit = limit }

    func document(_ text: String) -> MarkdownDocument {
        if let hit = cache.object(forKey: text as NSString) { return hit.document }
        let parsed = parseMarkdown(text)
        cache.setObject(Box(parsed), forKey: text as NSString)
        return parsed
    }
}

/// Highlight spans by language and code, so a redraw does not lex again.
final class SyntaxCache: @unchecked Sendable {
    static let shared = SyntaxCache()
    private final class Box {
        let spans: [SyntaxSpan]
        init(_ spans: [SyntaxSpan]) { self.spans = spans }
    }
    private let cache = NSCache<NSString, Box>()

    init(limit: Int = 200) { cache.countLimit = limit }

    func spans(_ code: String, language: String?) -> [SyntaxSpan] {
        let key = ((language ?? "") + "\u{0}" + code) as NSString
        if let hit = cache.object(forKey: key) { return hit.spans }
        let spans = SyntaxHighlighter.spans(code, language: language)
        cache.setObject(Box(spans), forKey: key)
        return spans
    }
}
