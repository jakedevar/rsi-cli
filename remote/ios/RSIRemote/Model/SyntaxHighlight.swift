import Foundation

// A small, dependency-free syntax highlighter for fenced code blocks (#503).
// It is a lexer, not a parser: comments, strings, numbers, keywords, types and
// calls, which is what a phone screen needs. Total: any text in, spans out whose
// concatenation is exactly the input; an unknown language is plain.

enum SyntaxKind: Sendable, Equatable {
    case plain, keyword, string, comment, number, type, function, added, removed, meta
}

struct SyntaxSpan: Equatable, Sendable {
    let kind: SyntaxKind
    let text: String
}

enum SyntaxHighlighter {
    /// Longer code is shown plain: the lexer is linear, but a phone should not
    /// build tens of thousands of attributed runs.
    static let maxCharacters = 20_000

    /// The language a fence names (`swift`, `rs`, `c++`, `Shell`...), or nil.
    static func canonicalLanguage(_ name: String?) -> String? {
        guard let name else { return nil }
        let key = name.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: ". {}"))
        return aliases[key]
    }

    /// Spans covering `code` exactly, in order.
    static func spans(_ code: String, language: String?) -> [SyntaxSpan] {
        guard !code.isEmpty else { return [] }
        guard code.count <= maxCharacters, let lang = canonicalLanguage(language) else {
            return [SyntaxSpan(kind: .plain, text: code)]
        }
        switch lang {
        case "diff": return diffSpans(code)
        case "html": return htmlSpans(code)
        default: break
        }
        guard let spec = specs[lang] else { return [SyntaxSpan(kind: .plain, text: code)] }
        var lexer = Lexer(Array(code), spec)
        return lexer.run()
    }

    // MARK: Languages

    private static let aliases: [String: String] = [
        "swift": "swift", "rust": "rust", "rs": "rust", "go": "go", "golang": "go",
        "js": "js", "javascript": "js", "jsx": "js", "mjs": "js", "cjs": "js",
        "ts": "js", "typescript": "js", "tsx": "js", "java": "java", "kotlin": "kotlin", "kt": "kotlin",
        "c": "c", "h": "c", "cpp": "c", "c++": "c", "cc": "c", "cxx": "c", "hpp": "c",
        "objc": "c", "objective-c": "c", "cs": "csharp", "csharp": "csharp", "c#": "csharp",
        "python": "python", "py": "python", "python3": "python", "ruby": "ruby", "rb": "ruby",
        "bash": "shell", "sh": "shell", "shell": "shell", "zsh": "shell", "console": "shell",
        "shellsession": "shell", "terminal": "shell", "fish": "shell",
        "sql": "sql", "postgres": "sql", "postgresql": "sql", "sqlite": "sql", "mysql": "sql",
        "json": "json", "jsonc": "json", "json5": "json", "yaml": "yaml", "yml": "yaml",
        "toml": "toml", "css": "css", "scss": "css", "html": "html", "xml": "html", "svg": "html",
        "diff": "diff", "patch": "diff", "dockerfile": "docker", "docker": "docker",
    ]

    private static func words(_ list: String) -> Set<String> {
        Set(list.split(separator: " ").map(String.init))
    }

    private static let specs: [String: Spec] = {
        let cLike = Spec(lineComments: ["//"], block: ("/*", "*/"))
        var swift = cLike
        swift.keywords = words(
            "associatedtype class deinit enum extension fileprivate func import init inout internal let open operator private protocol public rethrows static struct subscript typealias var break case catch continue default defer do else fallthrough for guard if in repeat return throw switch where while as await false is nil self Self super throws true try async actor some any nonisolated final lazy override mutating weak unowned indirect convenience required")
        swift.triple = true
        swift.attributes = true
        var rust = cLike
        rust.keywords = words(
            "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while union")
        rust.charQuote = true
        rust.macros = true
        var go = cLike
        go.keywords = words(
            "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var true false nil iota")
        go.backtick = true
        go.charQuote = true
        var js = cLike
        js.keywords = words(
            "async await break case catch class const continue debugger default delete do else export extends false finally for from function if import in instanceof interface let new null of return static super switch this throw true try type typeof undefined var void while with yield enum implements private protected public readonly abstract as declare namespace")
        js.singleQuote = true
        js.backtick = true
        var java = cLike
        java.keywords = words(
            "abstract assert boolean break byte case catch char class const continue default do double else enum extends final finally float for goto if implements import instanceof int interface long native new package private protected public return short static strictfp super switch synchronized this throw throws transient try void volatile while true false null var record")
        java.charQuote = true
        java.attributes = true
        var kotlin = cLike
        kotlin.keywords = words(
            "as break class continue do else false for fun if in interface is null object package return super this throw true try typealias val var when while by catch constructor finally import init abstract annotation companion const data enum final inline inner internal lateinit open operator out override private protected public sealed suspend vararg")
        kotlin.charQuote = true
        kotlin.triple = true
        kotlin.attributes = true
        var c = cLike
        c.keywords = words(
            "auto break case char const continue default do double else enum extern float for goto if inline int long register return short signed sizeof static struct switch typedef union unsigned void volatile while bool true false NULL nullptr class namespace template typename public private protected virtual override new delete using this try catch throw constexpr noexcept operator")
        c.charQuote = true
        c.directives = true
        var csharp = cLike
        csharp.keywords = words(
            "abstract as base bool break byte case catch char checked class const continue decimal default delegate do double else enum event explicit extern false finally fixed float for foreach goto if implicit in int interface internal is lock long namespace new null object operator out override params private protected public readonly ref return sbyte sealed short sizeof static string struct switch this throw true try typeof uint ulong unchecked unsafe ushort using var virtual void volatile while async await record")
        csharp.charQuote = true
        var python = Spec(lineComments: ["#"])
        python.keywords = words(
            "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield match case self")
        python.singleQuote = true
        python.triple = true
        python.attributes = true
        var ruby = Spec(lineComments: ["#"])
        ruby.keywords = words(
            "alias and begin break case class def do else elsif end ensure false for if in module next nil not or redo rescue retry return self super then true undef unless until when while yield")
        ruby.singleQuote = true
        var shell = Spec(lineComments: ["#"])
        shell.keywords = words(
            "if then else elif fi for while until do done case esac in function select time return exit break continue export local readonly unset declare source alias echo cd set shift trap eval exec")
        shell.singleQuote = true
        shell.shellVars = true
        shell.commentNeedsBoundary = true
        var sql = Spec(lineComments: ["--"], block: ("/*", "*/"))
        sql.keywords = words(
            "select from where and or not insert into values update set delete create table alter drop index primary key foreign references join left right inner outer full cross on group by order having limit offset as distinct union all case when then else end null is in like between exists count sum avg min max begin commit rollback with returning default unique check constraint view database using asc desc")
        sql.singleQuote = true
        sql.caseInsensitive = true
        var json = Spec()
        json.keywords = words("true false null")
        json.jsonKeys = true
        var yaml = Spec(lineComments: ["#"])
        yaml.keywords = words("true false null yes no on off")
        yaml.singleQuote = true
        yaml.keyFollow = ":"
        yaml.commentNeedsBoundary = true
        var toml = Spec(lineComments: ["#"])
        toml.keywords = words("true false")
        toml.singleQuote = true
        toml.keyFollow = "="
        var css = Spec(block: ("/*", "*/"))
        css.keywords = words("important inherit initial none auto")
        css.singleQuote = true
        css.keyFollow = ":"
        css.hexColor = true
        css.attributes = true
        var docker = Spec(lineComments: ["#"])
        docker.keywords = words(
            "FROM RUN CMD LABEL EXPOSE ENV ADD COPY ENTRYPOINT VOLUME USER WORKDIR ARG ONBUILD STOPSIGNAL HEALTHCHECK SHELL AS")
        docker.singleQuote = true
        docker.shellVars = true
        docker.commentNeedsBoundary = true
        return [
            "swift": swift, "rust": rust, "go": go, "js": js, "java": java, "kotlin": kotlin, "c": c,
            "csharp": csharp, "python": python, "ruby": ruby, "shell": shell, "sql": sql, "json": json,
            "yaml": yaml, "toml": toml, "css": css, "docker": docker,
        ]
    }()

    // MARK: Diff and markup

    private static func diffSpans(_ code: String) -> [SyntaxSpan] {
        var spans: [SyntaxSpan] = []
        let lines = code.components(separatedBy: "\n")
        for (index, line) in lines.enumerated() {
            let text = index == lines.count - 1 ? line : line + "\n"
            let kind: SyntaxKind
            if line.hasPrefix("+++") || line.hasPrefix("---") || line.hasPrefix("diff ")
                || line.hasPrefix("index ")
            {
                kind = .meta
            } else if line.hasPrefix("@@") {
                kind = .keyword
            } else if line.hasPrefix("+") {
                kind = .added
            } else if line.hasPrefix("-") {
                kind = .removed
            } else {
                kind = .plain
            }
            if !text.isEmpty { spans.append(SyntaxSpan(kind: kind, text: text)) }
        }
        return spans
    }

    private static func htmlSpans(_ code: String) -> [SyntaxSpan] {
        let chars = Array(code)
        var out = SpanBuilder()
        var i = 0
        let n = chars.count
        func starts(_ s: String, at: Int) -> Bool {
            let p = Array(s)
            return at + p.count <= n && Array(chars[at..<(at + p.count)]) == p
        }
        while i < n {
            if starts("<!--", at: i) {
                var j = i + 4
                while j < n, !starts("-->", at: j) { j += 1 }
                j = min(n, j + 3)
                out.add(.comment, chars[i..<j])
                i = j
            } else if chars[i] == "<", i + 1 < n, chars[i + 1].isLetter || chars[i + 1] == "/" || chars[i + 1] == "!" || chars[i + 1] == "?" {
                var j = i + 1
                while j < n, chars[j] == "/" || chars[j] == "!" || chars[j] == "?" { j += 1 }
                out.add(.plain, chars[i..<j])
                var k = j
                while k < n, chars[k].isLetter || chars[k].isNumber || chars[k] == "-" || chars[k] == ":" { k += 1 }
                out.add(.keyword, chars[j..<k])
                j = k
                // Attributes up to the closing '>'.
                while j < n, chars[j] != ">" {
                    if chars[j] == "\"" || chars[j] == "'" {
                        let q = chars[j]
                        var e = j + 1
                        while e < n, chars[e] != q { e += 1 }
                        e = min(n, e + 1)
                        out.add(.string, chars[j..<e])
                        j = e
                    } else if chars[j].isLetter || chars[j] == "_" {
                        var e = j
                        while e < n, chars[e].isLetter || chars[e].isNumber || "-_:.".contains(chars[e]) { e += 1 }
                        out.add(.type, chars[j..<e])
                        j = e
                    } else {
                        out.add(.plain, chars[j..<(j + 1)])
                        j += 1
                    }
                }
                if j < n {
                    out.add(.plain, chars[j..<(j + 1)])
                    j += 1
                }
                i = j
            } else {
                out.add(.plain, chars[i..<(i + 1)])
                i += 1
            }
        }
        return out.finished
    }
}

// MARK: - Lexer

private struct Spec {
    var lineComments: [String] = []
    var block: (String, String)?
    var keywords: Set<String> = []
    /// `'` opens a (single-line) string.
    var singleQuote = false
    /// `'` is a character literal or a lifetime, not a string.
    var charQuote = false
    var backtick = false
    var triple = false
    var caseInsensitive = false
    /// A word followed by this character is a key (`name:` in YAML, `name =` in TOML).
    var keyFollow: Character?
    var jsonKeys = false
    var shellVars = false
    var commentNeedsBoundary = false
    var hexColor = false
    /// `@name` is an attribute or decorator.
    var attributes = false
    /// `name!` is a macro call.
    var macros = false
    /// `#include` style directives.
    var directives = false
}

private struct SpanBuilder {
    var spans: [SyntaxSpan] = []
    private var kind: SyntaxKind?
    private var text = ""

    mutating func add(_ k: SyntaxKind, _ slice: ArraySlice<Character>) {
        guard !slice.isEmpty else { return }
        if kind != k {
            flush()
            kind = k
        }
        text.append(contentsOf: slice)
    }

    mutating func flush() {
        if let kind, !text.isEmpty { spans.append(SyntaxSpan(kind: kind, text: text)) }
        text = ""
    }

    var finished: [SyntaxSpan] {
        var copy = self
        copy.flush()
        return copy.spans
    }
}

private struct Lexer {
    let chars: [Character]
    let spec: Spec
    var out = SpanBuilder()

    init(_ chars: [Character], _ spec: Spec) {
        self.chars = chars
        self.spec = spec
    }

    private func isIdent(_ c: Character) -> Bool { c.isLetter || c.isNumber || c == "_" }

    private func matches(_ pattern: String, at i: Int) -> Bool {
        var j = i
        for c in pattern {
            guard j < chars.count, chars[j] == c else { return false }
            j += 1
        }
        return true
    }

    private func lineEnd(from i: Int) -> Int {
        var j = i
        while j < chars.count, chars[j] != "\n" { j += 1 }
        return j
    }

    private func nextNonSpace(from i: Int) -> Int {
        var j = i
        while j < chars.count, chars[j] == " " || chars[j] == "\t" { j += 1 }
        return j
    }

    mutating func run() -> [SyntaxSpan] {
        var i = 0
        let n = chars.count
        outer: while i < n {
            let c = chars[i]
            if let (open, close) = spec.block, matches(open, at: i) {
                var j = i + open.count
                while j < n, !matches(close, at: j) { j += 1 }
                j = min(n, j + close.count)
                out.add(.comment, chars[i..<j])
                i = j
                continue
            }
            for prefix in spec.lineComments where matches(prefix, at: i) {
                if spec.commentNeedsBoundary, i > 0, !chars[i - 1].isWhitespace { continue }
                let j = lineEnd(from: i)
                out.add(.comment, chars[i..<j])
                i = j
                continue outer
            }
            // Strings.
            if c == "\"" || (c == "'" && spec.singleQuote) || (c == "`" && spec.backtick) {
                if spec.triple, matches(String(repeating: String(c), count: 3), at: i) {
                    var j = i + 3
                    while j < n, !matches(String(repeating: String(c), count: 3), at: j) {
                        j += chars[j] == "\\" ? 2 : 1
                    }
                    j = min(n, j + 3)
                    out.add(.string, chars[i..<j])
                    i = j
                    continue
                }
                let multiline = c == "`"
                var j = i + 1
                while j < n {
                    if chars[j] == "\\" {
                        j += 2
                        continue
                    }
                    if chars[j] == c {
                        j += 1
                        break
                    }
                    if chars[j] == "\n", !multiline { break }
                    j += 1
                }
                j = min(n, j)
                var kind = SyntaxKind.string
                if spec.jsonKeys {
                    let k = nextNonSpace(from: j)
                    if k < n, chars[k] == ":" { kind = .type }
                }
                out.add(kind, chars[i..<j])
                i = j
                continue
            }
            if c == "'" && spec.charQuote {
                if i + 2 < n, chars[i + 1] != "\\", chars[i + 1] != "'", chars[i + 1] != "\n", chars[i + 2] == "'" {
                    out.add(.string, chars[i..<(i + 3)])
                    i += 3
                    continue
                }
                if i + 1 < n, chars[i + 1] == "\\" {
                    var j = i + 2
                    while j < n, j < i + 10, chars[j] != "'", chars[j] != "\n" { j += 1 }
                    if j < n, chars[j] == "'" {
                        out.add(.string, chars[i..<(j + 1)])
                        i = j + 1
                        continue
                    }
                }
                out.add(.plain, chars[i..<(i + 1)])
                i += 1
                continue
            }
            // Numbers (not the tail of a name like `x1`).
            if c.isASCII, c.isNumber, i == 0 || !isIdent(chars[i - 1]) {
                var j = i + 1
                while j < n, isIdent(chars[j]) || (chars[j] == "." && j + 1 < n && chars[j + 1].isASCII && chars[j + 1].isNumber) {
                    j += 1
                }
                out.add(.number, chars[i..<j])
                i = j
                continue
            }
            if spec.hexColor, c == "#" {
                var j = i + 1
                while j < n, chars[j].isHexDigit { j += 1 }
                if [4, 5, 7, 9].contains(j - i), j >= n || !isIdent(chars[j]) {
                    out.add(.number, chars[i..<j])
                    i = j
                    continue
                }
            }
            if c == "$" && spec.shellVars {
                var j = i + 1
                if j < n, chars[j] == "{" {
                    while j < n, chars[j] != "}", chars[j] != "\n" { j += 1 }
                    j = min(n, j + 1)
                } else {
                    while j < n, isIdent(chars[j]) { j += 1 }
                }
                if j > i + 1 {
                    out.add(.type, chars[i..<j])
                    i = j
                    continue
                }
            }
            if (c == "@" && spec.attributes) || (c == "#" && spec.directives) {
                var j = i + 1
                while j < n, isIdent(chars[j]) || chars[j] == "." || chars[j] == "-" { j += 1 }
                if j > i + 1 {
                    out.add(c == "#" ? .keyword : .meta, chars[i..<j])
                    i = j
                    continue
                }
            }
            if c.isLetter || c == "_" || (c == "$" && !spec.shellVars) {
                var j = i + 1
                while j < n, isIdent(chars[j]) || chars[j] == "$" { j += 1 }
                let word = String(chars[i..<j])
                let key = spec.caseInsensitive ? word.lowercased() : word
                let after = nextNonSpace(from: j)
                var kind = SyntaxKind.plain
                if spec.keywords.contains(key) {
                    kind = .keyword
                } else if let follow = spec.keyFollow, after < n, chars[after] == follow,
                    follow != ":" || after + 1 >= n || chars[after + 1] == " " || chars[after + 1] == "\n"
                {
                    kind = .type
                } else if spec.macros, j < n, chars[j] == "!", j + 1 < n, "([{".contains(chars[j + 1]) {
                    kind = .function
                } else if j < n, chars[j] == "(" {
                    kind = .function
                } else if let first = word.first, first.isUppercase, word.count > 1 {
                    kind = .type
                }
                out.add(kind, chars[i..<j])
                i = j
                continue
            }
            out.add(.plain, chars[i..<(i + 1)])
            i += 1
        }
        return out.finished
    }
}
