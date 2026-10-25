import UIKit
import XCTest

@testable import RSIRemote

/// The block parser behind every message (#503): each element, and the
/// degradation path for everything malformed.
final class MarkdownParserTests: XCTestCase {
    private func blocks(_ text: String) -> [MarkdownBlock] { parseMarkdown(text).blocks }

    // MARK: Elements

    func testHeadingsOfEveryLevel() {
        let text = (1...6).map { String(repeating: "#", count: $0) + " Title \($0)" }.joined(separator: "\n")
        XCTAssertEqual(
            blocks(text), (1...6).map { .heading(level: $0, text: "Title \($0)") })
        XCTAssertEqual(blocks("## Closed ##"), [.heading(level: 2, text: "Closed")])
    }

    func testNonHeadingsStayText() {
        XCTAssertEqual(blocks("#hashtag"), [.paragraph("#hashtag")])
        XCTAssertEqual(blocks("####### seven"), [.paragraph("####### seven")])
        XCTAssertEqual(blocks("just text"), [.paragraph("just text")])
        XCTAssertEqual(blocks(""), [])
        XCTAssertEqual(blocks("   \n \n"), [])
    }

    func testParagraphsKeepLineBreaksAndSplitOnBlankLines() {
        XCTAssertEqual(blocks("one\ntwo\n\nthree"), [.paragraph("one\ntwo"), .paragraph("three")])
        XCTAssertEqual(blocks("a\r\nb\rc"), [.paragraph("a\nb\nc")])
    }

    func testBulletAndNumberedLists() {
        XCTAssertEqual(
            blocks("- a\n- b\n* c\n+ d"),
            [
                .list(
                    ordered: false, start: 0,
                    items: ["a", "b", "c", "d"].map { MarkdownListItem(blocks: [.paragraph($0)], checked: nil) })
            ])
        XCTAssertEqual(
            blocks("3. three\n4. four\n5) five"),
            [
                .list(
                    ordered: true, start: 3,
                    items: ["three", "four", "five"].map { MarkdownListItem(blocks: [.paragraph($0)], checked: nil) })
            ])
        // `**bold**` and `-5` are not list markers.
        XCTAssertEqual(blocks("**bold** start"), [.paragraph("**bold** start")])
        XCTAssertEqual(blocks("-5 degrees"), [.paragraph("-5 degrees")])
    }

    func testNestedListsAtTwoAndFourSpaces() {
        for indent in ["  ", "    "] {
            let doc = blocks("- a\n\(indent)- b\n\(indent)\(indent)- c\n- d")
            guard case .list(_, _, let items) = doc.first, items.count == 2 else {
                return XCTFail("indent \(indent.count): \(doc)")
            }
            XCTAssertEqual(items[1].blocks, [.paragraph("d")])
            guard case .list(_, _, let level2) = items[0].blocks.last, level2.count == 1,
                case .list(_, _, let level3) = level2[0].blocks.last
            else { return XCTFail("no nesting at indent \(indent.count): \(doc)") }
            XCTAssertEqual(level3[0].blocks, [.paragraph("c")])
        }
    }

    func testBulletsNestedUnderANumberedItemWithTwoSpaces() {
        let doc = blocks("1. step\n  - detail\n2. next")
        guard case .list(true, 1, let items) = doc.first, items.count == 2 else { return XCTFail("\(doc)") }
        guard case .list(false, _, let inner) = items[0].blocks.last else { return XCTFail("\(items[0])") }
        XCTAssertEqual(inner[0].blocks, [.paragraph("detail")])
    }

    func testTaskList() {
        let doc = blocks("- [x] done\n- [ ] open\n- plain")
        guard case .list(_, _, let items) = doc.first else { return XCTFail("\(doc)") }
        XCTAssertEqual(items.map(\.checked), [true, false, nil])
        XCTAssertEqual(items[0].blocks, [.paragraph("done")])
    }

    func testListItemWithContinuationAndCodeBlock() {
        let doc = blocks("- first\n  continues\n- second\n\n  ```sh\n  echo hi\n  ```\n\nafter")
        guard case .list(_, _, let items) = doc.first, items.count == 2 else { return XCTFail("\(doc)") }
        XCTAssertEqual(items[0].blocks, [.paragraph("first\ncontinues")])
        XCTAssertEqual(items[1].blocks, [.paragraph("second"), .code(language: "sh", text: "echo hi")])
        XCTAssertEqual(doc.last, .paragraph("after"))
    }

    func testBlockQuotesNest() {
        XCTAssertEqual(
            blocks("> quoted\n> more\n>> inner"),
            [.quote([.paragraph("quoted\nmore"), .quote([.paragraph("inner")])])])
        XCTAssertEqual(blocks("> a\nlazy"), [.quote([.paragraph("a\nlazy")])])
    }

    func testQuoteHoldsListsAndCode() {
        let doc = blocks("> - a\n> - b\n>\n> ```\n> x\n> ```")
        guard case .quote(let inner) = doc.first, inner.count == 2 else { return XCTFail("\(doc)") }
        XCTAssertEqual(inner[1], .code(language: nil, text: "x"))
    }

    func testRules() {
        XCTAssertEqual(blocks("a\n\n---\n\nb"), [.paragraph("a"), .rule, .paragraph("b")])
        XCTAssertEqual(blocks("***"), [.rule])
        XCTAssertEqual(blocks("_ _ _"), [.rule])
    }

    func testFencedCode() {
        XCTAssertEqual(
            blocks("```swift\nlet x = 1\n\nlet y = 2\n```"),
            [.code(language: "swift", text: "let x = 1\n\nlet y = 2")])
        XCTAssertEqual(blocks("~~~\nplain\n~~~"), [.code(language: nil, text: "plain")])
        // A longer fence holds a shorter one; the info string's first word is the language.
        XCTAssertEqual(
            blocks("````md {x}\n```\ninner\n```\n````"),
            [.code(language: "md", text: "```\ninner\n```")])
        // Markdown inside code stays code.
        XCTAssertEqual(blocks("```\n# not a heading\n- not a list\n```"), [.code(language: nil, text: "# not a heading\n- not a list")])
    }

    func testTableWithAlignmentsAndRaggedRows() {
        let doc = blocks("| A | B | C |\n| :-- | :-: | --: |\n| 1 | 2 |\n| x | y | z | extra |\n\nafter")
        guard case .table(let table) = doc.first else { return XCTFail("\(doc)") }
        XCTAssertEqual(table.header, ["A", "B", "C"])
        XCTAssertEqual(table.aligns, [.leading, .center, .trailing])
        XCTAssertEqual(table.rows, [["1", "2", ""], ["x", "y", "z"]])
        XCTAssertEqual(doc.last, .paragraph("after"))
    }

    func testTableWithoutOuterPipesEscapedPipesAndCodePipes() {
        let doc = blocks("a | b\n--- | ---\nx \\| y | `p|q`")
        guard case .table(let table) = doc.first else { return XCTFail("\(doc)") }
        XCTAssertEqual(table.header, ["a", "b"])
        XCTAssertEqual(table.rows, [["x | y", "`p|q`"]])
    }

    func testTableInterruptsAParagraph() {
        let doc = blocks("Results:\n| a | b |\n|---|---|\n| 1 | 2 |")
        XCTAssertEqual(doc.count, 2)
        XCTAssertEqual(doc.first, .paragraph("Results:"))
    }

    // MARK: Images

    func testImagesInTextAreSplitOut() {
        XCTAssertEqual(
            blocks("see ![a cat](https://example.com/c.png \"title\") here"),
            [.paragraph("see"), .image(alt: "a cat", url: "https://example.com/c.png"), .paragraph("here")])
        XCTAssertEqual(blocks("![](data:image/png;base64,AAAA)"), [.image(alt: "", url: "data:image/png;base64,AAAA")])
        XCTAssertEqual(
            blocks("[![badge](https://example.com/b.png)](https://example.com/page)"),
            [.image(alt: "badge", url: "https://example.com/b.png")])
        XCTAssertEqual(blocks("![a <b>](<https://example.com/x y.png>)").count, 1)
    }

    func testBareImageURLOnItsOwnLineIsAnImage() {
        XCTAssertEqual(
            blocks("look:\nhttps://example.com/p/cover.JPG\nbye"),
            [.paragraph("look:"), .image(alt: "", url: "https://example.com/p/cover.JPG"), .paragraph("bye")])
        // A URL inside a sentence, or not an image, is text.
        XCTAssertEqual(blocks("see https://example.com/a.png now"), [.paragraph("see https://example.com/a.png now")])
        XCTAssertEqual(blocks("https://example.com/page"), [.paragraph("https://example.com/page")])
    }

    func testImageSyntaxInCodeIsNotAnImage() {
        XCTAssertEqual(blocks("use `![a](b)` for images"), [.paragraph("use `![a](b)` for images")])
        XCTAssertEqual(blocks("```\n![a](https://example.com/b.png)\n```"), [.code(language: nil, text: "![a](https://example.com/b.png)")])
    }

    func testMalformedImagesStayText() {
        for text in ["![broken", "![alt](", "![alt](no close", "![alt] (url)", "![alt]()", "!["] {
            XCTAssertEqual(blocks(text), [.paragraph(text)], text)
        }
    }

    func testImageBudgetBoundsWork() {
        let many = (1...100).map { "![i\($0)](https://example.com/\($0).png)" }.joined(separator: " ")
        let images = blocks(many).filter { if case .image = $0 { true } else { false } }
        XCTAssertEqual(images.count, MarkdownLimits.maxImagesPerMessage)
    }

    // MARK: Degradation

    func testUnbalancedFenceRunsToTheEnd() {
        XCTAssertEqual(blocks("```\ncode"), [.code(language: nil, text: "code")])
        XCTAssertEqual(
            blocks("before\n```py\nx = 1\n\n| not | table"),
            [.paragraph("before"), .code(language: "py", text: "x = 1\n\n| not | table")])
        XCTAssertEqual(blocks("```` \n```\nstill open"), [.code(language: nil, text: "```\nstill open")])
        XCTAssertEqual(blocks("``` not ` a fence"), [.paragraph("``` not ` a fence")])
    }

    func testBrokenTablesAreText() {
        // No separator row.
        XCTAssertEqual(blocks("| a | b |\n| 1 | 2 |"), [.paragraph("| a | b |\n| 1 | 2 |")])
        // The separator has a different number of columns.
        XCTAssertEqual(blocks("| a | b |\n|---|"), [.paragraph("| a | b |\n|---|")])
        // A bad separator cell.
        XCTAssertEqual(blocks("| a | b |\n|--x|---|"), [.paragraph("| a | b |\n|--x|---|")])
        // Too many columns to draw.
        let wide = (0..<(MarkdownLimits.maxTableColumns + 1)).map { _ in "x" }
        let header = "|" + wide.joined(separator: "|") + "|"
        let sep = "|" + wide.map { _ in "-" }.joined(separator: "|") + "|"
        XCTAssertEqual(blocks(header + "\n" + sep).count, 1)
        if case .table = blocks(header + "\n" + sep)[0] { XCTFail("drew a table wider than the limit") }
        // A lone pipe line.
        XCTAssertEqual(blocks("|"), [.paragraph("|")])
    }

    func testLongTablesAreCutWithANotice() {
        let rows = (0..<(MarkdownLimits.maxTableRows + 50)).map { "| \($0) |" }.joined(separator: "\n")
        guard case .table(let table) = blocks("| n |\n|---|\n" + rows).first else { return XCTFail("no table") }
        XCTAssertEqual(table.rows.count, MarkdownLimits.maxTableRows + 1)
        XCTAssertEqual(table.rows.last, ["… 50 more rows"])
    }

    func testUnknownSyntaxIsPlainText() {
        for text in [
            "<div class=\"x\">html</div>", "$$ x^2 $$", "[ref]: http://example.com", "Term\n: definition",
            "<script>alert(1)</script>", "\u{0}\u{1}binary\u{7f}", "\\*escaped\\*",
        ] {
            let doc = blocks(text)
            XCTAssertFalse(doc.isEmpty, text)
            XCTAssertEqual(doc, [.paragraph(text)], text)
        }
    }

    func testDeepNestingIsCappedNotCrashed() {
        let quotes = String(repeating: "> ", count: 5_000) + "deep"
        let doc = blocks(quotes)
        XCTAssertFalse(doc.isEmpty)
        var depth = 0
        var node = doc.first
        while case .quote(let inner)? = node {
            depth += 1
            node = inner.first
        }
        XCTAssertLessThanOrEqual(depth, MarkdownLimits.maxDepth + 2)
        let lists = (0..<200).map { String(repeating: "  ", count: $0) + "- item \($0)" }.joined(separator: "\n")
        XCTAssertFalse(blocks(lists).isEmpty)
    }

    func testHugeInputIsBounded() {
        let line = "word **bold** `code` [l](https://example.com) - x | y\n"
        let huge = String(repeating: line, count: 20_000)  // ~1.1 MB
        let started = Date()
        let doc = parseMarkdown(huge)
        XCTAssertLessThan(Date().timeIntervalSince(started), 5)
        XCTAssertGreaterThan(doc.omittedCharacters, 0)
        XCTAssertLessThanOrEqual(doc.blocks.count, MarkdownLimits.maxBlocks)
        // One enormous line, and one enormous code block.
        XCTAssertEqual(parseMarkdown(String(repeating: "a", count: 500_000)).omittedCharacters, 500_000 - MarkdownLimits.maxCharacters)
        let code = "```\n" + String(repeating: "x = 1\n", count: 3_000) + "```"
        guard case .code(_, let text) = parseMarkdown(code).blocks.first else { return XCTFail("no code") }
        XCTAssertGreaterThan(text.count, 10_000)
    }

    func testNormalMessagesOmitNothing() {
        XCTAssertEqual(parseMarkdown(MarkdownFixture.text).omittedCharacters, 0)
        XCTAssertEqual(parseMarkdown(String(repeating: "x", count: 4096)).omittedCharacters, 0)
    }

    func testRandomMarkdownNeverTrapsAndBoundsHold() {
        let pieces = [
            "#", "##", " ", "  ", "    ", "\n", "\n\n", "-", "*", "+", "1.", "2)", ">", "|", "---", ":-:", "```", "~~~",
            "`", "![", "](", ")", "[", "]", "!", "a", "word", "**", "_", "\\", "<br>", "https://x.io/a.png", "[x]", "\t",
            "é", "😀", "\r", "\u{0}",
        ]
        var seed: UInt64 = 0x9E37_79B9_7F4A_7C15
        func next() -> Int {
            seed = seed &* 6_364_136_223_846_793_005 &+ 1_442_695_040_888_963_407
            return Int(seed >> 33)
        }
        for _ in 0..<1_500 {
            var text = ""
            for _ in 0..<(next() % 60) { text += pieces[next() % pieces.count] }
            let doc = parseMarkdown(text)
            XCTAssertLessThanOrEqual(doc.blocks.count, MarkdownLimits.maxBlocks)
            // The inline layer takes whatever a block holds.
            for block in doc.blocks { exercise(block) }
        }
    }

    private func exercise(_ block: MarkdownBlock) {
        switch block {
        case .paragraph(let text), .heading(_, let text): _ = sanitizedInline(text)
        case .list(_, _, let items): items.forEach { $0.blocks.forEach(exercise) }
        case .quote(let inner): inner.forEach(exercise)
        case .table(let table): (table.header + table.rows.flatMap { $0 }).forEach { _ = sanitizedInline($0) }
        case .image(_, let url): _ = ImagePolicy.classify(url)
        case .code(let language, let text): _ = SyntaxHighlighter.spans(text, language: language)
        case .rule: break
        }
    }

    // MARK: The fixture

    private func walk(_ blocks: [MarkdownBlock], _ visit: (MarkdownBlock) -> Void) {
        for block in blocks {
            visit(block)
            switch block {
            case .list(_, _, let items): items.forEach { walk($0.blocks, visit) }
            case .quote(let inner): walk(inner, visit)
            default: break
            }
        }
    }

    func testFixtureConversationHasEveryElement() {
        var headings = Set<Int>(), languages = Set<String>()
        var ordered = false, bullets = false, nestedList = false, task = false, nestedQuote = false, rule = false
        var tables: [MarkdownTable] = [], images: [String] = [], paragraphs = 0
        for event in MarkdownFixture.events {
            XCTAssertLessThanOrEqual(event.text.utf8.count, maxMessageBytes, "one event is bounded by the hub")
            let doc = parseMarkdown(event.text)
            XCTAssertEqual(doc.omittedCharacters, 0)
            walk(doc.blocks) { block in
                switch block {
                case .heading(let level, _): headings.insert(level)
                case .list(let isOrdered, _, let items):
                    if isOrdered { ordered = true } else { bullets = true }
                    if items.contains(where: { $0.checked != nil }) { task = true }
                    if items.contains(where: { $0.blocks.contains { if case .list = $0 { true } else { false } } }) {
                        nestedList = true
                    }
                case .quote(let inner):
                    if inner.contains(where: { if case .quote = $0 { true } else { false } }) { nestedQuote = true }
                case .code(let language, _): languages.insert(language ?? "none")
                case .table(let table): tables.append(table)
                case .image(_, let url): images.append(url)
                case .rule: rule = true
                case .paragraph: paragraphs += 1
                }
            }
        }
        XCTAssertEqual(headings, [1, 2, 3, 4])
        XCTAssertTrue(ordered && bullets && nestedList && task && nestedQuote && rule)
        XCTAssertEqual(languages, ["swift", "python", "json", "bash", "diff", "none"])
        XCTAssertEqual(tables.count, 1)
        XCTAssertEqual(tables[0].aligns, [.leading, .center, .trailing])
        XCTAssertEqual(tables[0].rows.count, 3)
        XCTAssertGreaterThanOrEqual(images.count, 6)
        XCTAssertGreaterThan(paragraphs, 4)
        // The fixture's images exercise every policy outcome.
        let kinds = images.map { ImagePolicy.classify($0) }
        XCTAssertEqual(kinds.filter { if case .inline = $0 { true } else { false } }.count, 1)
        XCTAssertEqual(kinds.filter { if case .remote = $0 { true } else { false } }.count, 2)
        XCTAssertEqual(kinds.filter { if case .blocked = $0 { true } else { false } }.count, 3)
    }

    @MainActor
    func testDemoLaunchValueSelectsAPart() {
        let all = 1 + MarkdownFixture.parts.count
        XCTAssertEqual(MarkdownDemoView.events(for: nil).count, all)
        XCTAssertEqual(MarkdownDemoView.events(for: "0").count, all)
        XCTAssertEqual(MarkdownDemoView.events(for: "yes").count, all)
        XCTAssertEqual(MarkdownDemoView.events(for: "99").count, all)
        XCTAssertEqual(MarkdownDemoView.events(for: "3").map(\.text), [MarkdownFixture.question, MarkdownFixture.parts[2]])
    }

    func testFixtureConversationGroupsIntoTurns() {
        let turns = ChatTranscript.build(MarkdownFixture.events)
        XCTAssertEqual(turns.map(\.role), [.user, .assistant])
        XCTAssertEqual(turns[1].parts.count, MarkdownFixture.parts.count)
    }
}

// MARK: - Inline

final class MarkdownInlineTests: XCTestCase {
    private func links(_ text: String) -> [String] {
        let rendered = sanitizedInline(text)
        return rendered.runs.compactMap { $0.link?.absoluteString }
    }

    func testSafeLinksStay() {
        XCTAssertEqual(links("[a](https://example.com/x)"), ["https://example.com/x"])
        XCTAssertEqual(links("[a](http://example.com)"), ["http://example.com"])
        XCTAssertEqual(links("[mail](mailto:a@example.com)"), ["mailto:a@example.com"])
    }

    func testUnsafeAndRelativeLinksAreDropped() {
        for target in ["javascript:alert(1)", "tel:5551234", "sms:5551234", "shortcuts://run-shortcut?name=x",
            "file:///etc/passwd", "rsi://x", "relative/path", "/abs/path", "data:text/html,hi", "https://"]
        {
            XCTAssertEqual(links("[click](\(target))"), [], target)
            XCTAssertEqual(String(sanitizedInline("[click](\(target))").characters), "click", target)
        }
    }

    func testBareWebAddressesBecomeLinks() {
        XCTAssertEqual(links("see https://example.com/status now"), ["https://example.com/status"])
        XCTAssertEqual(links("a http://example.com/a and https://example.org/b"), ["http://example.com/a", "https://example.org/b"])
        // Not a web address, not linked: a file name is not a site.
        XCTAssertEqual(links("open README.md and Notes.txt"), [])
        // Already a link, or code: left alone.
        XCTAssertEqual(links("[x](https://example.com/a) https://example.com/a"), ["https://example.com/a", "https://example.com/a"])
        XCTAssertEqual(links("`https://example.com/code`"), [])
    }

    func testLineBreakTagsAndPlainFallback() {
        XCTAssertEqual(String(sanitizedInline("a<br>b<br/>c").characters), "a\nb\nc")
        XCTAssertEqual(String(sanitizedInline("**unclosed").characters), "**unclosed")
        XCTAssertEqual(String(sanitizedInline("").characters), "")
    }

    func testStylesSurvive() {
        let rendered = sanitizedInline("**b** *i* ~~s~~ `c`")
        let intents = rendered.runs.compactMap(\.inlinePresentationIntent)
        XCTAssertTrue(intents.contains(.stronglyEmphasized))
        XCTAssertTrue(intents.contains(.emphasized))
        XCTAssertTrue(intents.contains(.strikethrough))
        XCTAssertTrue(intents.contains(.code))
    }
}

// MARK: - Syntax highlighting

final class SyntaxHighlightTests: XCTestCase {
    private func kinds(_ code: String, _ language: String) -> [(SyntaxKind, String)] {
        SyntaxHighlighter.spans(code, language: language).map { ($0.kind, $0.text) }
    }

    private func has(_ code: String, _ language: String, _ kind: SyntaxKind, _ text: String) -> Bool {
        SyntaxHighlighter.spans(code, language: language).contains { $0.kind == kind && $0.text == text }
    }

    func testSpansAlwaysRebuildTheCode() {
        let samples = [
            MarkdownFixture.swiftCode, "let s = \"unterminated\nnext line", "/* open comment", "'", "\"", "`", "```",
            "x = '''open", "<a href=\"x", "<!-- open", "{\"a\": [1, 2, {\"b\": null}]}", "é😀 \u{0} \t", "",
            "@@ -1 +1 @@\n-a\n+b\n c", "#include <x>\nint main() { return 0; }",
        ]
        let languages = [nil, "swift", "rust", "go", "js", "java", "kotlin", "c", "csharp", "python", "ruby", "shell", "sql", "json", "yaml", "toml", "css", "html", "diff", "dockerfile", "nonsense"]
        for sample in samples {
            for language in languages {
                let spans = SyntaxHighlighter.spans(sample, language: language)
                XCTAssertEqual(spans.map(\.text).joined(), sample, "\(language ?? "nil"): \(sample.prefix(30))")
            }
        }
    }

    func testRandomCodeNeverTrapsAndRebuilds() {
        let pieces = ["\"", "'", "`", "\\", "/*", "*/", "//", "#", "<", ">", "!", "-->", "<!--", "{", "}", "\n", " ", "a", "1", "0x", "@", "$", "(", "e", "\"\"\"", "=", ":"]
        var seed: UInt64 = 42
        func next() -> Int {
            seed = seed &* 6_364_136_223_846_793_005 &+ 1
            return Int(seed >> 33)
        }
        let languages = ["swift", "rust", "js", "python", "shell", "sql", "json", "yaml", "css", "html", "diff", "c", "go"]
        for _ in 0..<600 {
            var code = ""
            for _ in 0..<(next() % 50) { code += pieces[next() % pieces.count] }
            let language = languages[next() % languages.count]
            XCTAssertEqual(SyntaxHighlighter.spans(code, language: language).map(\.text).joined(), code, code)
        }
    }

    func testSwiftTokens() {
        let code = "// note\nlet n = 42\nstruct Box { var s = \"hi\" }\nprint(n)"
        XCTAssertTrue(has(code, "swift", .comment, "// note"))
        XCTAssertTrue(has(code, "swift", .keyword, "let"))
        XCTAssertTrue(has(code, "swift", .number, "42"))
        XCTAssertTrue(has(code, "swift", .type, "Box"))
        XCTAssertTrue(has(code, "swift", .string, "\"hi\""))
        XCTAssertTrue(has(code, "swift", .function, "print"))
    }

    func testPythonTripleQuotesDecoratorsAndComments() {
        let code = "@cache\ndef f(x):\n    \"\"\"doc\n    string\"\"\"\n    return x  # tail"
        XCTAssertTrue(has(code, "python", .meta, "@cache"))
        XCTAssertTrue(has(code, "python", .keyword, "def"))
        XCTAssertTrue(has(code, "py", .string, "\"\"\"doc\n    string\"\"\""))
        XCTAssertTrue(has(code, "python", .comment, "# tail"))
    }

    func testJsonKeysAreNotValues() {
        let code = "{\"key\": \"value\", \"n\": -1.5e3, \"ok\": true}"
        XCTAssertTrue(has(code, "json", .type, "\"key\""))
        XCTAssertTrue(has(code, "json", .string, "\"value\""))
        XCTAssertTrue(has(code, "json", .keyword, "true"))
        XCTAssertTrue(has(code, "json", .number, "1.5e3"))
    }

    func testRustLifetimesAreNotStrings() {
        let code = "fn f<'a>(x: &'a str) -> char { 'z' }"
        XCTAssertTrue(has(code, "rust", .string, "'z'"))
        XCTAssertFalse(SyntaxHighlighter.spans(code, language: "rust").contains { $0.kind == .string && $0.text.contains("a>") })
        XCTAssertTrue(has("println!(\"x\");", "rust", .function, "println"))
    }

    func testShellCommentsNeedAWordBoundaryAndVariablesAreMarked() {
        XCTAssertTrue(has("echo a#b # real", "bash", .comment, "# real"))
        XCTAssertFalse(has("echo a#b", "bash", .comment, "#b"))
        XCTAssertTrue(has("echo \"$HOME\" ${PATH}", "sh", .type, "${PATH}"))
        XCTAssertTrue(has("echo $USER", "zsh", .type, "$USER"))
    }

    func testDiffLines() {
        let spans = SyntaxHighlighter.spans("@@ -1 +1 @@\n-old\n+new\n same", language: "diff")
        XCTAssertEqual(spans.map(\.kind), [.keyword, .removed, .added, .plain])
        XCTAssertEqual(spans.map(\.text).joined(), "@@ -1 +1 @@\n-old\n+new\n same")
    }

    func testHtmlTagsAttributesAndComments() {
        let code = "<!-- c --><a href=\"x\" class='y'>t</a>"
        XCTAssertTrue(has(code, "html", .comment, "<!-- c -->"))
        XCTAssertTrue(has(code, "html", .keyword, "a"))
        XCTAssertTrue(has(code, "html", .type, "href"))
        XCTAssertTrue(has(code, "xml", .string, "\"x\""))
    }

    func testSQLIsCaseInsensitive() {
        XCTAssertTrue(has("SELECT count(*) from t where x = 'a' -- c", "sql", .keyword, "SELECT"))
        XCTAssertTrue(has("select 1 FROM t", "sql", .keyword, "FROM"))
        XCTAssertTrue(has("select 1 -- c", "SQL", .comment, "-- c"))
    }

    func testAStringEndsAtTheLineSoAStrayQuoteDoesNotEatTheRest() {
        let spans = SyntaxHighlighter.spans("let a = \"oops\nlet b = 2", language: "swift")
        XCTAssertTrue(spans.contains { $0.kind == .number && $0.text == "2" })
    }

    func testUnknownAndOversizedCodeIsPlain() {
        XCTAssertEqual(SyntaxHighlighter.spans("let x = 1", language: "klingon"), [SyntaxSpan(kind: .plain, text: "let x = 1")])
        XCTAssertEqual(SyntaxHighlighter.spans("let x = 1", language: nil), [SyntaxSpan(kind: .plain, text: "let x = 1")])
        XCTAssertEqual(SyntaxHighlighter.spans("", language: "swift"), [])
        let big = String(repeating: "let x = 1\n", count: 3_000)
        XCTAssertEqual(SyntaxHighlighter.spans(big, language: "swift"), [SyntaxSpan(kind: .plain, text: big)])
    }

    func testLanguageAliases() {
        for (alias, canonical) in [("Swift", "swift"), ("RS", "rust"), ("c++", "c"), ("TypeScript", "js"), ("py", "python"), ("Shell", "shell"), ("yml", "yaml"), ("patch", "diff")] {
            XCTAssertEqual(SyntaxHighlighter.canonicalLanguage(alias), canonical, alias)
        }
        XCTAssertNil(SyntaxHighlighter.canonicalLanguage("text"))
        XCTAssertNil(SyntaxHighlighter.canonicalLanguage(nil))
    }

    func testHighlightingLargeCodeIsFast() {
        let code = String(repeating: "func f(_ x: Int) -> String { return \"a\" + String(x) } // c\n", count: 300)
        let started = Date()
        XCTAssertGreaterThan(SyntaxHighlighter.spans(code, language: "swift").count, 1_000)
        XCTAssertLessThan(Date().timeIntervalSince(started), 3)
    }

    func testEveryKindReadsOnTheCodeBackgroundInEveryThemeAndAppearance() {
        let kinds: [SyntaxKind] = [.plain, .keyword, .string, .comment, .number, .type, .function, .added, .removed, .meta]
        for base in RemoteTheme.all {
            for theme in [base, base.lightVariant] {
                for kind in kinds {
                    let ratio = theme.syntaxColor(kind).contrast(with: theme.codeBg)
                    XCTAssertGreaterThanOrEqual(ratio, 4.0, "\(theme.key) light=\(theme.isLight) \(kind)")
                }
            }
        }
    }

    func testEnsuringContrastOnlyMovesWhatItMust() {
        let dark = RGB(30, 30, 46)
        XCTAssertEqual(RGB(255, 255, 255).ensuringContrast(on: dark), RGB(255, 255, 255))
        let moved = RGB(60, 60, 90).ensuringContrast(on: dark)
        XCTAssertNotEqual(moved, RGB(60, 60, 90))
        XCTAssertGreaterThanOrEqual(moved.contrast(with: dark), 4.0)
    }
}

// MARK: - Copy

final class CodeCopyTests: XCTestCase {
    private final class Recorder: TextPasteboard {
        var written: [String] = []
        func write(_ text: String) { written.append(text) }
    }

    func testCopyWritesExactlyTheCode() {
        let recorder = Recorder()
        let code = "line 1\n\tindented \"quoted\" é😀\n\ntrailing newline\n"
        XCTAssertTrue(CodeCopy.copy(code, to: recorder))
        XCTAssertEqual(recorder.written, [code])
    }

    func testCopyReachesARealPasteboard() {
        let name = UIPasteboard.Name("rsi.test.\(UUID().uuidString)")
        guard let board = UIPasteboard(name: name, create: true) else { return XCTFail("no pasteboard") }
        defer { UIPasteboard.remove(withName: name) }
        let code = "swift build && echo \"done\"\n"
        CodeCopy.copy(code, to: board)
        XCTAssertEqual(board.string, code)
    }

    func testTheCodeAMessageHoldsIsWhatTheBlockCopies() {
        // The block copies its parsed text, not the folded preview: a long block copies every line.
        let lines = (1...400).map { "line \($0)" }
        guard case .code(_, let text)? = parseMarkdown("```\n" + lines.joined(separator: "\n") + "\n```").blocks.first
        else { return XCTFail("no code block") }
        let recorder = Recorder()
        CodeCopy.copy(text, to: recorder)
        XCTAssertEqual(recorder.written.first, lines.joined(separator: "\n"))
        XCTAssertGreaterThan(lines.count, CodeBlockView.foldAbove)
    }
}
