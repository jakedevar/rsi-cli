//! The Python-source half of the fixture-Git guard (#2574, #225, #469).
//!
//! `scripts/tests/test_fixture_git_guard.py` scanned every `scripts/tests/*.py` fixture with
//! `ast` and refused unsafe Git use; its shell port (`test_fixture_git_guard.sh`) scans only
//! `*.sh`. Python fixtures are still committed data until each is ported, and scanning them
//! as TEXT needs no interpreter, so this module is the same scanner over a small tokenizer:
//!
//! * an executable `subprocess` (or `os.system`/`os.popen`) Git call with no `-C` (an
//!   implicit working directory, whether or not `cwd=` is passed);
//! * a `--git-dir`/`--work-tree` global option on such a call;
//! * a direct `git -C <dir> <mutating subcommand>` (use `scratch_git`);
//! * `cd <dir> && git ...` in a shell string (or a `bash -c` list);
//! * in a module that runs fixture Git, a `tempfile.TemporaryDirectory()`/`mkdtemp()` with no
//!   `dir=` (it lands under `TMPDIR`, which an RSI sandbox points inside itself).
//!
//! Only executable calls are inspected: a call inside a string literal (fixture program source),
//! a mock that compares an argument vector, and `scratch_git(...)` itself are not Git calls.
//! `# fixture-git: allow <reason>` on the line (or a comment-only line directly above) exempts it.
use crate::re::{Lazy, Re};

const SUBPROCESS_FUNCTIONS: [&str; 5] = ["run", "call", "check_call", "check_output", "Popen"];
const READ_ONLY_SUBCOMMANDS: [&str; 20] = [
    "rev-parse",
    "hash-object",
    "check-ignore",
    "ls-files",
    "ls-tree",
    "diff",
    "log",
    "show",
    "status",
    "cat-file",
    "merge-base",
    "rev-list",
    "show-ref",
    "symbolic-ref",
    "for-each-ref",
    "describe",
    "grep",
    "blame",
    "name-rev",
    "var",
];
const IMPLICIT: &str =
    "Git runs on an implicit working directory (pass the repository with scratch_git or `-C`)";
const OVERRIDE: &str = "a --git-dir/--work-tree override moves Git off the validated directory";
const UNDECODABLE: &str = "a string escape this scanner cannot decode (\\N{...}) is unsupported source: spell the text plainly";
const TMPDIR: &str =
    "a Git fixture module allocates a directory under TMPDIR (use scratch_tempdir)";

static ALLOW: Lazy = Lazy::new(|| Re::new(r"#\s*fixture-git:\s*allow\s+(\S.*)$"));
static SHELL_CD_GIT: Lazy = Lazy::new(|| Re::new(r"\bcd\b[^\n]*?(&&|;|\|\|)\s*(\w+=\S+\s+)*git\b"));
static SHELL_COMMAND_GIT: Lazy =
    Lazy::new(|| Re::new(r"(?:^|[;&|(`]|\$\()\s*(?:\w+=\S+\s+)*git\s+(?P<rest>\S+)"));
static BARE_GIT: Lazy = Lazy::new(|| Re::new(r"\bgit\b"));

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Name(String),
    /// A string literal's text (escapes decoded; an f-string keeps only its literal parts).
    Str {
        text: String,
        fstring: bool,
        /// The source of every f-string replacement field: it is code, so it is scanned (#478).
        fields: Vec<String>,
        /// An escape this scanner cannot decode (`\N{...}`): unsupported source, refused.
        undecodable: bool,
    },
    Number,
    Punct(char),
    /// Two-character comparison/augmented operators (`==`, `!=`, `<=`, `+=` ...).
    Op,
    Newline,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: Kind,
    line: usize,
    /// First token of a logical line (or after `;`): where an assignment can start.
    stmt_start: bool,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Up to `max` digits of `radix` taken from the front of `chars`, as a number.
fn take_digits(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    radix: u32,
    max: usize,
) -> Option<(u32, usize)> {
    let (mut value, mut count) = (0u32, 0usize);
    while count < max {
        let Some(digit) = chars.peek().and_then(|c| c.to_digit(radix)) else {
            break;
        };
        value = value * radix + digit;
        count += 1;
        chars.next();
    }
    (count > 0).then_some((value, count))
}

/// Decode a non-raw string body the way Python does (#478): `\n`-style escapes, `\xHH`, octal,
/// `\uXXXX`, `\UXXXXXXXX`; an unknown escape keeps its backslash. `\N{...}` needs the Unicode
/// name table, so it (and any escape that names no character) is reported undecodable and the
/// caller refuses the source rather than guessing.
fn unescape(body: &str) -> (String, bool) {
    let mut out = String::with_capacity(body.len());
    let mut undecodable = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(next) = chars.next() else {
            continue;
        };
        let decoded = match next {
            '\n' => None,
            'n' => Some('\n'),
            't' => Some('\t'),
            'r' => Some('\r'),
            'a' => Some('\x07'),
            'b' => Some('\x08'),
            'f' => Some('\x0c'),
            'v' => Some('\x0b'),
            '\\' | '\'' | '"' => Some(next),
            '0'..='7' => {
                let mut value = next.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    let Some(digit) = chars.peek().and_then(|c| c.to_digit(8)) else {
                        break;
                    };
                    value = value * 8 + digit;
                    chars.next();
                }
                match char::from_u32(value) {
                    Some(decoded) => Some(decoded),
                    None => {
                        undecodable = true;
                        None
                    }
                }
            }
            'x' | 'u' | 'U' => {
                let width = match next {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                match take_digits(&mut chars, 16, width) {
                    Some((value, count)) if count == width => match char::from_u32(value) {
                        Some(decoded) => Some(decoded),
                        None => {
                            undecodable = true;
                            None
                        }
                    },
                    // A malformed numeric escape is a Python syntax error: never a Git word.
                    _ => {
                        undecodable = true;
                        None
                    }
                }
            }
            'N' => {
                undecodable = true;
                if chars.peek() == Some(&'{') {
                    for skipped in chars.by_ref() {
                        if skipped == '}' {
                            break;
                        }
                    }
                }
                None
            }
            other => {
                out.push('\\');
                out.push(other);
                None
            }
        };
        if let Some(decoded) = decoded {
            out.push(decoded);
        }
    }
    (out, undecodable)
}

/// The literal text of an f-string body (`{{`/`}}` unescaped) and the source of each `{expr}`
/// replacement field, which is code and so is returned for scanning, not discarded (#478).
fn fstring_parts(body: &str, raw: bool) -> (String, Vec<String>) {
    let chars: Vec<char> = body.chars().collect();
    let (mut out, mut fields) = (String::new(), Vec::new());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if !raw && chars.get(i + 1) == Some(&'N') && chars.get(i + 2) == Some(&'{') => {
                // `\N{NAME}` is an escape, not a replacement field.
                while i < chars.len() && chars[i] != '}' {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            '\\' if !raw && i + 1 < chars.len() && chars[i + 1] != '{' && chars[i + 1] != '}' => {
                out.push('\\');
                out.push(chars[i + 1]);
                i += 2;
            }
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let (mut depth, start) = (0usize, i + 1);
                while i < chars.len() {
                    match chars[i] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                fields.push(chars[start..i.min(chars.len())].iter().collect());
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    (out, fields)
}

fn tokenize(source: &str) -> Vec<Tok> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens: Vec<Tok> = Vec::new();
    let (mut i, mut line, mut depth) = (0usize, 1usize, 0i32);
    let mut stmt_start = true;
    let push = |tokens: &mut Vec<Tok>, kind: Kind, line: usize, stmt_start: &mut bool| {
        tokens.push(Tok {
            kind,
            line,
            stmt_start: *stmt_start,
        });
        *stmt_start = false;
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                if depth <= 0 {
                    push(&mut tokens, Kind::Newline, line, &mut stmt_start);
                    stmt_start = true;
                }
                line += 1;
                i += 1;
            }
            ' ' | '\t' | '\r' | '\x0c' => i += 1,
            '\\' if chars.get(i + 1) == Some(&'\n') => {
                line += 1;
                i += 2;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ';' if depth <= 0 => {
                push(&mut tokens, Kind::Punct(';'), line, &mut stmt_start);
                stmt_start = true;
                i += 1;
            }
            c if is_ident_start(c) => {
                let start = i;
                while i < chars.len() && is_ident(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let lower = word.to_ascii_lowercase();
                let prefixed = matches!(
                    lower.as_str(),
                    "r" | "b" | "u" | "f" | "rb" | "br" | "fr" | "rf"
                );
                if prefixed && matches!(chars.get(i), Some('\'' | '"')) {
                    let raw = lower.contains('r');
                    let fstring = lower.contains('f');
                    let (kind, next, newlines) = read_string(&chars, i, raw, fstring);
                    push(&mut tokens, kind, line, &mut stmt_start);
                    line += newlines;
                    i = next;
                } else {
                    push(&mut tokens, Kind::Name(word), line, &mut stmt_start);
                }
            }
            '\'' | '"' => {
                let (kind, next, newlines) = read_string(&chars, i, false, false);
                push(&mut tokens, kind, line, &mut stmt_start);
                line += newlines;
                i = next;
            }
            c if c.is_ascii_digit() => {
                while i < chars.len() && (is_ident(chars[i]) || chars[i] == '.') {
                    i += 1;
                }
                push(&mut tokens, Kind::Number, line, &mut stmt_start);
            }
            '=' | '!' | '<' | '>' | '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^' | '@' | ':'
                if chars.get(i + 1) == Some(&'=') =>
            {
                push(&mut tokens, Kind::Op, line, &mut stmt_start);
                i += 2;
            }
            _ => {
                match c {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth -= 1,
                    _ => {}
                }
                push(&mut tokens, Kind::Punct(c), line, &mut stmt_start);
                i += 1;
            }
        }
    }
    tokens
}

/// Read the string literal whose opening quote is at `open`. Returns its `Str` token, the
/// index after the closing quote and how many newlines the literal spans.
fn read_string(chars: &[char], open: usize, raw: bool, fstring: bool) -> (Kind, usize, usize) {
    let quote = chars[open];
    let triple = chars.get(open + 1) == Some(&quote) && chars.get(open + 2) == Some(&quote);
    let mut i = open + if triple { 3 } else { 1 };
    let start = i;
    let mut newlines = 0usize;
    while i < chars.len() {
        if chars[i] == '\\' {
            if chars.get(i + 1) == Some(&'\n') {
                newlines += 1;
            }
            i += 2;
            continue;
        }
        if chars[i] == '\n' {
            newlines += 1;
            if !triple {
                break;
            }
        }
        if chars[i] == quote
            && (!triple || (chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote)))
        {
            break;
        }
        i += 1;
    }
    let end = i.min(chars.len());
    let body: String = chars[start..end].iter().collect();
    let next = if i >= chars.len() {
        chars.len()
    } else {
        i + if triple { 3 } else { 1 }
    };
    let (literal, fields) = if fstring {
        fstring_parts(&body, raw)
    } else {
        (body, Vec::new())
    };
    let (text, undecodable) = if raw {
        (literal, false)
    } else {
        unescape(&literal)
    };
    let kind = Kind::Str {
        text,
        fstring,
        fields,
        undecodable,
    };
    (kind, next, newlines)
}

fn is_punct(token: &Tok, want: char) -> bool {
    token.kind == Kind::Punct(want)
}

/// The index of the bracket closing the one at `open`.
fn matching_close(tokens: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            Kind::Punct('(' | '[' | '{') => depth += 1,
            Kind::Punct(')' | ']' | '}') => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split at top-level commas; an empty trailing piece is dropped.
fn split_top(tokens: &[Tok]) -> Vec<&[Tok]> {
    let mut pieces = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            Kind::Punct('(' | '[' | '{') => depth += 1,
            Kind::Punct(')' | ']' | '}') => depth -= 1,
            Kind::Punct(',') if depth == 0 => {
                pieces.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < tokens.len() {
        pieces.push(&tokens[start..]);
    }
    pieces
}

fn has_top_comma(tokens: &[Tok]) -> bool {
    let mut depth = 0i32;
    tokens.iter().any(|token| match token.kind {
        Kind::Punct('(' | '[' | '{') => {
            depth += 1;
            false
        }
        Kind::Punct(')' | ']' | '}') => {
            depth -= 1;
            false
        }
        Kind::Punct(',') => depth == 0,
        _ => false,
    })
}

/// The string an expression of only adjacent string literals denotes, with whether it is an
/// f-string (a `JoinedStr`); `None` for any other expression.
fn string_expression(tokens: &[Tok]) -> Option<(String, bool)> {
    if tokens.is_empty() {
        return None;
    }
    let (mut text, mut fstring) = (String::new(), false);
    for token in tokens {
        match &token.kind {
            Kind::Str {
                text: part,
                fstring: f,
                ..
            } => {
                text.push_str(part);
                fstring |= f;
            }
            _ => return None,
        }
    }
    Some((text, fstring))
}

/// `_text`: a plain (non-f) string constant.
fn plain_text(tokens: &[Tok]) -> Option<String> {
    string_expression(tokens).and_then(|(text, f)| (!f).then_some(text))
}

/// A list/tuple literal's element slices, when the expression is exactly one.
fn sequence_elements(tokens: &[Tok]) -> Option<Vec<&[Tok]>> {
    let first = tokens.first()?;
    let close = matching_close(tokens, 0)?;
    if close != tokens.len() - 1 {
        return None;
    }
    let inner = &tokens[1..close];
    if is_punct(first, '[') || (is_punct(first, '(') && (inner.is_empty() || has_top_comma(inner)))
    {
        return Some(split_top(inner));
    }
    None
}

/// Strip redundant parentheses around a non-tuple expression: `("a" "b")` is a string.
fn unwrap_parens(tokens: &[Tok]) -> &[Tok] {
    let mut current = tokens;
    while current.len() >= 2
        && is_punct(&current[0], '(')
        && matching_close(current, 0) == Some(current.len() - 1)
        && !has_top_comma(&current[1..current.len() - 1])
        && current.len() > 2
    {
        current = &current[1..current.len() - 1];
    }
    current
}

/// `(problem, subcommand)` for the Git argument vector after the leading `git`.
fn classify_argv(elements: &[Option<String>]) -> Option<String> {
    let (mut explicit, mut subcommand, mut index) = (false, None::<String>, 0usize);
    while index < elements.len() {
        let Some(value) = elements[index].as_deref() else {
            break;
        };
        if value.starts_with("--git-dir") || value.starts_with("--work-tree") {
            return Some(OVERRIDE.to_string());
        }
        if value == "-c" || value == "--config-env" {
            index += 2;
            continue;
        }
        if value == "-C" {
            explicit = true;
            index += 2;
            continue;
        }
        if value.starts_with('-') {
            index += 1;
            continue;
        }
        subcommand = Some(value.to_string());
        break;
    }
    if !explicit {
        return Some(IMPLICIT.to_string());
    }
    match subcommand {
        Some(sub) if READ_ONLY_SUBCOMMANDS.contains(&sub.as_str()) => None,
        other => Some(format!(
            "`git -C <dir> {}` can change a repository without the outside-root validation (use scratch_git)",
            other.as_deref().unwrap_or("<unknown>")
        )),
    }
}

fn shell_string_problem(text: &str) -> Option<String> {
    if SHELL_CD_GIT.is_match(text) {
        return Some(
            "`cd <dir> && git ...` runs Git wherever the cd leaves the shell (use `git -C <dir>`)"
                .to_string(),
        );
    }
    for captures in SHELL_COMMAND_GIT.find_all(text) {
        let rest = captures.name("rest").map_or("", |m| m.as_str());
        if !rest.starts_with("-C") {
            return Some(
                "shell Git without `-C` runs on an implicit working directory".to_string(),
            );
        }
    }
    None
}

/// Name -> the element slices of the list/tuple literal last assigned to it.
fn argv_assignments(tokens: &[Tok]) -> Vec<(String, Vec<&[Tok]>)> {
    let mut found: Vec<(String, Vec<&[Tok]>)> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let Kind::Name(name) = &token.kind else {
            continue;
        };
        if !token.stmt_start || !tokens.get(index + 1).is_some_and(|t| is_punct(t, '=')) {
            continue;
        }
        // `a = b = [...]` assigns every Name target, as `ast.Assign.targets` does (#478).
        let mut names = vec![name.clone()];
        let mut start = index + 2;
        while let (
            Some(Tok {
                kind: Kind::Name(more),
                ..
            }),
            Some(eq),
        ) = (tokens.get(start), tokens.get(start + 1))
        {
            if !is_punct(eq, '=') {
                break;
            }
            names.push(more.clone());
            start += 2;
        }
        let Some(open) = tokens.get(start) else {
            continue;
        };
        if !matches!(open.kind, Kind::Punct('[' | '(')) {
            continue;
        }
        let Some(close) = matching_close(tokens, start) else {
            continue;
        };
        let ends = tokens
            .get(close + 1)
            .is_none_or(|t| matches!(t.kind, Kind::Newline) || is_punct(t, ';'));
        if !ends {
            continue;
        }
        if let Some(elements) = sequence_elements(&tokens[start..=close]) {
            for target in &names {
                found.retain(|(known, _)| known != target);
                found.push((target.clone(), elements.clone()));
            }
        }
    }
    found
}

struct Call<'a> {
    module: &'a str,
    function: &'a str,
    line: usize,
    positional: Vec<&'a [Tok]>,
    keywords: Vec<(&'a str, &'a [Tok])>,
}

/// Every `name(...)` / `module.name(...)` call whose callee is a plain dotted name.
fn calls(tokens: &[Tok]) -> Vec<Call<'_>> {
    let mut found = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, Kind::Name(_)) {
            continue;
        }
        if !tokens.get(index + 1).is_some_and(|t| is_punct(t, '(')) {
            continue;
        }
        // The dotted chain ending here.
        let mut start = index;
        while start >= 2
            && is_punct(&tokens[start - 1], '.')
            && matches!(tokens[start - 2].kind, Kind::Name(_))
        {
            start -= 2;
        }
        if start >= 1 {
            match &tokens[start - 1].kind {
                Kind::Punct('.') => continue,
                Kind::Name(keyword) if keyword == "def" || keyword == "class" => continue,
                _ => {}
            }
        }
        let names: Vec<&str> = tokens[start..=index]
            .iter()
            .filter_map(|t| match &t.kind {
                Kind::Name(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        let (module, function) = match names.as_slice() {
            [only] => ("", *only),
            [module, function] => (*module, *function),
            _ => ("", ""),
        };
        let Some(close) = matching_close(tokens, index + 1) else {
            continue;
        };
        let (mut positional, mut keywords) = (Vec::new(), Vec::new());
        for piece in split_top(&tokens[index + 2..close]) {
            match piece {
                [name, eq, ..]
                    if matches!(name.kind, Kind::Name(_))
                        && is_punct(eq, '=')
                        && piece.len() > 2 =>
                {
                    if let Kind::Name(key) = &name.kind {
                        keywords.push((key.as_str(), &piece[2..]));
                    }
                }
                _ => positional.push(piece),
            }
        }
        found.push(Call {
            module,
            function,
            line: tokens[start].line,
            positional,
            keywords,
        });
    }
    found
}

fn allowed(lines: &[&str], number: usize) -> bool {
    if (1..=lines.len()).contains(&number) && ALLOW.is_match(lines[number - 1]) {
        return true;
    }
    if number >= 2 && number - 2 < lines.len() {
        let above = lines[number - 2];
        return above.trim_start().starts_with('#') && ALLOW.is_match(above);
    }
    false
}

/// Every f-string replacement field of every stream is code: its own token stream, scanned like
/// the module (nested f-strings recurse; every token keeps the f-string's line) (#478).
fn gather_field_streams(streams: &mut Vec<Vec<Tok>>) {
    let mut next = 0;
    while next < streams.len() {
        let mut pending = Vec::new();
        for token in &streams[next] {
            if let Kind::Str { fields, .. } = &token.kind {
                for field in fields {
                    let mut inner = tokenize(field);
                    for inner_token in &mut inner {
                        inner_token.line = token.line;
                    }
                    pending.push(inner);
                }
            }
        }
        streams.extend(pending);
        next += 1;
    }
}

/// `(line, message)` for every unsafe fixture Git use in one Python module, sorted by line.
#[must_use]
pub fn python_violations(source: &str) -> Vec<(usize, String)> {
    let main = tokenize(source);
    let mut streams = vec![main];
    gather_field_streams(&mut streams);
    let lines: Vec<&str> = source.lines().collect();
    let assignments = argv_assignments(&streams[0]);
    let mut found: Vec<(usize, String)> = Vec::new();
    for token in streams.iter().flatten() {
        if matches!(
            token.kind,
            Kind::Str {
                undecodable: true,
                ..
            }
        ) {
            found.push((token.line, UNDECODABLE.to_string()));
        }
    }
    let mut uses_git = false;
    let mut allocators: Vec<Call<'_>> = Vec::new();
    for call in streams.iter().flat_map(|stream| calls(stream)) {
        if call.function == "scratch_git" {
            uses_git = true;
            continue;
        }
        if matches!(call.function, "TemporaryDirectory" | "mkdtemp")
            && matches!(call.module, "tempfile" | "")
        {
            allocators.push(call);
            continue;
        }
        let is_subprocess = (call.module == "subprocess"
            && SUBPROCESS_FUNCTIONS.contains(&call.function))
            || (call.module.is_empty()
                && matches!(call.function, "Popen" | "check_output" | "check_call"));
        let is_shell = call.module == "os" && matches!(call.function, "system" | "popen");
        if !is_subprocess && !is_shell {
            continue;
        }
        let argument = call.positional.first().copied().or_else(|| {
            call.keywords
                .iter()
                .find(|(key, _)| *key == "args")
                .map(|(_, value)| *value)
        });
        let Some(mut argument) = argument else {
            continue;
        };
        // A Name argument is the list/tuple literal last assigned to it, when there is one.
        let mut resolved: Option<Vec<&[Tok]>> = None;
        if let [single] = argument {
            if let Kind::Name(name) = &single.kind {
                if let Some((_, elements)) = assignments.iter().find(|(known, _)| known == name) {
                    resolved = Some(elements.clone());
                }
            }
        }
        argument = unwrap_parens(argument);
        let elements = resolved.or_else(|| sequence_elements(argument));
        let mut problem = None;
        match elements {
            Some(elements) if !elements.is_empty() => {
                let texts: Vec<Option<String>> = elements.iter().map(|e| plain_text(e)).collect();
                match texts[0].as_deref() {
                    Some("git") => {
                        uses_git = true;
                        problem = classify_argv(&texts[1..]);
                    }
                    Some("bash" | "sh" | "zsh")
                        if texts.len() >= 3
                            && matches!(texts[1].as_deref(), Some("-c" | "-lc")) =>
                    {
                        if let Some((shell, _)) = string_expression(elements[2]) {
                            if !shell.is_empty() {
                                problem = shell_string_problem(&shell);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                if let Some((shell, _)) = string_expression(argument) {
                    if BARE_GIT.is_match(&shell) {
                        uses_git = true;
                        problem = shell_string_problem(&shell);
                    }
                }
            }
        }
        if let Some(problem) = problem {
            found.push((call.line, problem));
        }
    }
    for call in &allocators {
        let has_dir = call.keywords.iter().any(|(key, value)| {
            *key == "dir" && !matches!(value, [only] if only.kind == Kind::Name("None".to_string()))
        }) || call.positional.len() >= 3;
        if uses_git && !has_dir {
            found.push((call.line, TMPDIR.to_string()));
        }
    }
    found.sort();
    found
        .into_iter()
        .filter(|(line, _)| !allowed(&lines, *line))
        .collect()
}

/// Every `# fixture-git: allow <reason>` line of a source: `(line number, reason)`.
#[must_use]
pub fn allowances(source: &str) -> Vec<(usize, String)> {
    source
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let captures = ALLOW.search(line)?;
            Some((index + 1, captures.get(1)?.as_str().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn messages(source: &str) -> Vec<String> {
        python_violations(source)
            .into_iter()
            .map(|(_, message)| message)
            .collect()
    }

    // One test per legacy `test_fixture_git_guard.py` Python-specific case (parent 4e22ec478^).

    #[test]
    fn bare_subprocess_git_with_cwd_is_rejected() {
        let source = "import subprocess\ndef git(root, *args):\n    subprocess.run(['git', *args], cwd=root, check=True)\n";
        assert_eq!(messages(source), [IMPLICIT]);
    }

    #[test]
    fn bare_subprocess_git_through_a_variable_is_rejected() {
        let source = "import subprocess\ncommand = ['git', 'commit', '-qm', 'x']\nsubprocess.check_output(command)\n";
        assert_eq!(messages(source).len(), 1);
    }

    #[test]
    fn git_options_before_the_subcommand_do_not_hide_a_bare_call() {
        let source = "import subprocess\nsubprocess.run(['git', '-c', 'user.name=t', 'commit', '-qm', 'x'])\n";
        assert_eq!(messages(source).len(), 1);
    }

    #[test]
    fn direct_mutating_git_c_is_rejected() {
        for subcommand in [
            "commit",
            "reset",
            "checkout",
            "init",
            "worktree",
            "update-ref",
        ] {
            let source = format!(
                "import subprocess\nsubprocess.run(['git', '-C', str(repo), '{subcommand}'])\n"
            );
            let found = messages(&source);
            assert_eq!(found.len(), 1, "{subcommand}");
            assert!(
                found[0].contains("use scratch_git"),
                "{subcommand}: {found:?}"
            );
        }
    }

    #[test]
    fn git_c_with_an_unknown_subcommand_is_rejected() {
        let source = "import subprocess\nsubprocess.run(['git', '-C', str(repo), *args])\n";
        assert_eq!(messages(source).len(), 1);
    }

    #[test]
    fn git_dir_and_work_tree_options_are_rejected() {
        for option in ["--git-dir=x/.git", "--work-tree=x"] {
            let source =
                format!("import subprocess\nsubprocess.run(['git', '{option}', 'status'])\n");
            assert!(messages(&source)[0].contains("override"), "{option}");
        }
    }

    #[test]
    fn shell_string_cd_and_git_is_rejected() {
        for command in [
            "'cd \"$scratch\" && git commit -qm x'",
            "'cd scratch; git reset --hard'",
            "'git commit -qm x'",
        ] {
            let source = format!("import os\nos.system({command})\n");
            assert_eq!(messages(&source).len(), 1, "{command}");
        }
        assert_eq!(
            messages("import subprocess\nsubprocess.run('cd x && git add .', shell=True)\n").len(),
            1
        );
        assert_eq!(
            messages("import subprocess\nsubprocess.run(['bash', '-c', 'cd x && git add .'])\n")
                .len(),
            1
        );
    }

    #[test]
    fn a_fixture_module_allocating_under_tmpdir_is_rejected() {
        let source = "import tempfile\nfrom scratch_git import scratch_git\ntemp = tempfile.TemporaryDirectory(prefix='x-')\nscratch_git(temp.name, 'init')\n";
        assert_eq!(messages(source), [TMPDIR]);
        assert_eq!(
            messages(&source.replace(
                "tempfile.TemporaryDirectory(prefix='x-')",
                "tempfile.mkdtemp()"
            ))
            .len(),
            1
        );
    }

    #[test]
    fn scratch_git_and_scratch_tempdir_are_accepted() {
        let source = "from scratch_git import scratch_git, scratch_tempdir\nwith scratch_tempdir(prefix='x-') as root:\n    scratch_git(root, 'init', '-q')\n    scratch_git(root, '-c', 'user.name=t', 'commit', '--allow-empty', '-qm', 'x')\n";
        assert_eq!(messages(source), Vec::<String>::new());
        // A tempdir with an explicit dir= is fine in a Git module.
        let with_dir = "import tempfile\nfrom scratch_git import scratch_git\ntemp = tempfile.mkdtemp(dir=parent)\nscratch_git(temp, 'init')\n";
        assert_eq!(messages(with_dir), Vec::<String>::new());
    }

    #[test]
    fn explicit_read_only_git_c_is_accepted() {
        let source = "import subprocess\nsubprocess.run(['git', '-C', str(ROOT), 'check-ignore', '-q', 'a'])\nsubprocess.check_output(['git', '-c', 'core.quotepath=off', '-C', str(ROOT), 'ls-files'])\n";
        assert_eq!(messages(source), Vec::<String>::new());
    }

    #[test]
    fn a_module_without_git_may_allocate_under_tmpdir() {
        let source = "import tempfile\ntemp = tempfile.TemporaryDirectory(prefix='plain-')\n";
        assert_eq!(messages(source), Vec::<String>::new());
    }

    #[test]
    fn inert_source_strings_and_mock_matching_are_not_git_calls() {
        let source = concat!(
            "import subprocess\n",
            "FAKE = '''\\nimport subprocess\\nsubprocess.run(['git', 'commit'], cwd='.')\\n'''\n",
            "WRAPPER = 'cd x && git commit'\n",
            "def fake_run(command, **kwargs):\n",
            "    if command[:2] == ('git', 'rev-parse'):\n",
            "        return subprocess.CompletedProcess(command, 0, '', '')\n",
            "    if command[0] == 'git':\n",
            "        raise AssertionError(command)\n",
            "SHIMS = (('git', '/bin/git'),)\n",
            "subprocess.run(['which', 'git'])\n",
            "subprocess.run(['echo', 'git commit'])\n",
        );
        assert_eq!(messages(source), Vec::<String>::new());
        // A real triple-quoted fixture program that merely CONTAINS a call is inert too.
        let program =
            "PROGRAM = \"\"\"\nimport subprocess\nsubprocess.run(['git', 'commit'])\n\"\"\"\n";
        assert_eq!(messages(program), Vec::<String>::new());
    }

    #[test]
    fn an_allow_pragma_with_a_reason_exempts_only_its_line() {
        let source = "import subprocess\nsubprocess.run(['git', 'status'])  # fixture-git: allow product wrapper reads the caller cwd\nsubprocess.run(['git', 'status'])\n";
        let lines: Vec<usize> = python_violations(source)
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        assert_eq!(lines, [3]);
        // A comment-only line directly above exempts the next call, and only that one.
        let above = "import subprocess\n# fixture-git: allow product wrapper reads the caller cwd\nsubprocess.run(['git', 'status'])\nsubprocess.run(['git', 'status'])\n";
        let lines: Vec<usize> = python_violations(above)
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        assert_eq!(lines, [4]);
        // A pragma without a reason is not an allowance.
        assert!(allowances("x = 1  # fixture-git: allow\n").is_empty());
    }

    #[test]
    fn multi_line_calls_and_implicit_string_concatenation_are_read() {
        let source = "import subprocess\nsubprocess.run(\n    ['git', 'commit',\n     '-qm', 'x'],\n    check=True,\n)\nsubprocess.run('cd x && '\n               'git add .', shell=True)\n";
        let found = python_violations(source);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].0, 2);
        assert_eq!(found[1].0, 7);
    }

    /// Every committed `scripts/tests/*.py` fixture is scanned as text (no interpreter runs).
    #[test]
    fn every_python_fixture_runs_git_in_an_explicit_validated_repository() {
        let tests = Path::new(rsi_common::manifest_dir!()).join("../../scripts/tests");
        let mut scanned = 0usize;
        let mut report = Vec::new();
        let mut names: Vec<_> = std::fs::read_dir(&tests)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == "py"))
            .collect();
        names.sort();
        for path in &names {
            let source = std::fs::read_to_string(path).unwrap();
            scanned += 1;
            for (line, message) in python_violations(&source) {
                report.push(format!(
                    "{}:{line}: {message}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
        }
        assert!(
            report.is_empty(),
            "unsafe fixture Git (use scripts/scratch_git.py): {report:#?}"
        );
        // Not vacuous while Python fixtures remain: the scan found the fixtures that run Git.
        assert!(scanned == 0 || names.iter().any(|p| p.ends_with("test_scratch_git.py")));
    }

    /// The exact allowance inventory across `.py` and `.sh` fixtures, with real reasons.
    #[test]
    fn the_only_allowance_is_the_scratch_git_helper_test_on_its_fake_assigned_repo() {
        let tests = Path::new(rsi_common::manifest_dir!()).join("../../scripts/tests");
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&tests).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !(name.ends_with(".py") || name.ends_with(".sh"))
                || name == "test_fixture_git_guard.sh"
            {
                continue;
            }
            for (line, reason) in allowances(&std::fs::read_to_string(&path).unwrap()) {
                assert!(
                    reason.split_whitespace().count() >= 3,
                    "{name}:{line} allowance needs a real reason"
                );
                files.push(name.clone());
            }
        }
        files.sort();
        // scratch_git refuses a protected root by design, and test_scratch_git builds that
        // root on purpose; no other fixture needs an exception. Exactly one allowance exists.
        assert!(
            files == ["test_scratch_git.py"] || files == ["test_scratch_git.sh"],
            "{files:?}"
        );
    }
}
