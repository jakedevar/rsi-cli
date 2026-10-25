//! A small SQLite lexer and scalar-expression parser for the isolated-migration
//! proof (#457, #462).
//!
//! The legacy proof executed the literal DDL in a private SQLite connection, so
//! SQLite itself rejected every malformed or unresolvable statement. This port has
//! no embedded SQLite: it accepts only the subset of SQLite's grammar parsed here,
//! and anything it cannot parse in full is "not proven isolated", which keeps every
//! rewind caller. Accepting a string SQLite rejects would drop required tests, so
//! each rule below is a subset of SQLite's: when in doubt the parser says no.
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Tok {
    /// An unquoted word or number, ASCII-lowercased (SQLite folds ASCII case only).
    Word(String),
    /// A quoted identifier (`"x"`, `` `x` `` or `[x]`), ASCII-lowercased.
    Ident(String),
    Str,
    /// A two-character operator written without a gap: `<= >= <> != == || << >>`.
    Op(&'static str),
    Punct(char),
}

const TWO_CHAR_OPERATORS: [&str; 8] = ["<=", ">=", "<>", "!=", "==", "||", "<<", ">>"];

fn name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Length of the numeric literal that starts at `from` (digits, a fraction, an
/// exponent or a hex literal); 0 when none does.
fn number_span(b: &[u8], from: usize) -> usize {
    let digits = |mut i: usize, f: fn(&u8) -> bool| {
        while b.get(i).is_some_and(f) {
            i += 1;
        }
        i
    };
    let mut i = from;
    if b.get(i) == Some(&b'0')
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        return digits(i + 2, u8::is_ascii_hexdigit) - from;
    }
    i = digits(i, u8::is_ascii_digit);
    let mut mantissa = i > from;
    if b.get(i) == Some(&b'.') {
        let fraction_end = digits(i + 1, u8::is_ascii_digit);
        mantissa |= fraction_end > i + 1;
        i = fraction_end;
    }
    if !mantissa {
        return 0;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if b.get(j).is_some_and(u8::is_ascii_digit) {
            i = digits(j, u8::is_ascii_digit);
        }
    }
    i - from
}

/// Whether a whole word is one numeric literal.
pub(super) fn number_like(word: &str) -> bool {
    let b = word.as_bytes();
    !b.is_empty() && number_span(b, 0) == b.len()
}

/// Tokens with their byte spans; comments are skipped. `None` on an unterminated
/// quote, which no migration the proof accepts can contain.
pub(super) fn sql_tokens(sql: &str) -> Option<Vec<(Tok, usize, usize)>> {
    let b = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let from = i;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i = sql[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
        } else if c == b'\'' || c == b'"' || c == b'`' {
            i += 1;
            loop {
                let q = sql[i..].find(c as char)? + i;
                if b.get(q + 1) == Some(&c) {
                    i = q + 2;
                } else {
                    i = q + 1;
                    break;
                }
            }
            let text = sql[from + 1..i - 1]
                .replace(&format!("{0}{0}", c as char), &(c as char).to_string());
            tokens.push((
                if c == b'\'' {
                    Tok::Str
                } else {
                    Tok::Ident(text.to_ascii_lowercase())
                },
                from,
                i,
            ));
        } else if c == b'[' {
            i = sql[i..].find(']')? + i + 1;
            tokens.push((
                Tok::Ident(sql[from + 1..i - 1].to_ascii_lowercase()),
                from,
                i,
            ));
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            i += number_span(b, i);
            // SQLite reads identifier characters glued to a number as one illegal
            // token; keep them in the word so it is no number and resolves nowhere.
            while i < b.len() && name_byte(b[i]) {
                i += 1;
            }
            tokens.push((Tok::Word(sql[from..i].to_ascii_lowercase()), from, i));
        } else if name_byte(c) {
            while i < b.len() && name_byte(b[i]) {
                i += 1;
            }
            tokens.push((Tok::Word(sql[from..i].to_ascii_lowercase()), from, i));
        } else if let Some(op) = b
            .get(i + 1)
            .and_then(|n| TWO_CHAR_OPERATORS.iter().find(|o| o.as_bytes() == [c, *n]))
        {
            i += 2;
            tokens.push((Tok::Op(*op), from, i));
        } else {
            i += 1;
            tokens.push((Tok::Punct(c as char), from, i));
        }
    }
    Some(tokens)
}

pub(super) fn is_word(tok: &Tok, word: &str) -> bool {
    matches!(tok, Tok::Word(w) if w == word)
}

pub(super) fn name_of(tok: &Tok) -> Option<&str> {
    match tok {
        Tok::Word(w) | Tok::Ident(w) => Some(w),
        _ => None,
    }
}

/// Every SQLite keyword. An unquoted one is not a plain name (SQLite reads it as
/// a keyword or falls back to an identifier only in positions this grammar does
/// not model), so a name that is one must be quoted to be proven.
const KEYWORDS: &[&str] = &[
    "abort",
    "action",
    "add",
    "after",
    "all",
    "alter",
    "always",
    "analyze",
    "and",
    "as",
    "asc",
    "attach",
    "autoincrement",
    "before",
    "begin",
    "between",
    "by",
    "cascade",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "commit",
    "conflict",
    "constraint",
    "create",
    "cross",
    "current",
    "current_date",
    "current_time",
    "current_timestamp",
    "database",
    "default",
    "deferrable",
    "deferred",
    "delete",
    "desc",
    "detach",
    "distinct",
    "do",
    "drop",
    "each",
    "else",
    "end",
    "escape",
    "except",
    "exclude",
    "exclusive",
    "exists",
    "explain",
    "fail",
    "filter",
    "first",
    "following",
    "for",
    "foreign",
    "from",
    "full",
    "generated",
    "glob",
    "group",
    "groups",
    "having",
    "if",
    "ignore",
    "immediate",
    "in",
    "index",
    "indexed",
    "initially",
    "inner",
    "insert",
    "instead",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "key",
    "last",
    "left",
    "like",
    "limit",
    "match",
    "materialized",
    "natural",
    "no",
    "not",
    "nothing",
    "notnull",
    "null",
    "nulls",
    "of",
    "offset",
    "on",
    "or",
    "order",
    "others",
    "outer",
    "over",
    "partition",
    "plan",
    "pragma",
    "preceding",
    "primary",
    "query",
    "raise",
    "range",
    "recursive",
    "references",
    "regexp",
    "reindex",
    "release",
    "rename",
    "replace",
    "restrict",
    "returning",
    "right",
    "rollback",
    "row",
    "rows",
    "savepoint",
    "select",
    "set",
    "table",
    "temp",
    "temporary",
    "then",
    "ties",
    "to",
    "transaction",
    "trigger",
    "unbounded",
    "union",
    "unique",
    "update",
    "using",
    "vacuum",
    "values",
    "view",
    "virtual",
    "when",
    "where",
    "window",
    "with",
    "without",
];

/// Keywords SQLite lets stand for an identifier wherever a column may be named.
const FALLBACK_NAMES: &[&str] = &["action", "first", "key", "last", "offset"];

/// An object or column name this grammar can resolve: a non-empty quoted
/// identifier, or an unquoted word that starts like a name and is no keyword.
pub(super) fn plain_name(tok: &Tok) -> Option<&str> {
    match tok {
        Tok::Ident(name) => (!name.is_empty()).then_some(name.as_str()),
        Tok::Word(w) => {
            let first = *w.as_bytes().first()?;
            let starts = first.is_ascii_alphabetic() || first == b'_' || first >= 0x80;
            (starts && (!KEYWORDS.contains(&w.as_str()) || FALLBACK_NAMES.contains(&w.as_str())))
                .then_some(w.as_str())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The expression allowlist (#462)
//
// Parsing "everything SQLite parses" is whack-a-mole: each review found one more
// construct SQLite rejects (`GLOB ... ESCAPE`, expression trees past the depth
// limit, too many function arguments). The proof is therefore an ALLOWLIST. An
// expression is proven only when it is built solely from the constructs below, in
// the sizes below; anything else is "not proven isolated" and keeps every rewind
// caller. Over-refusal only selects extra tests (the safe direction); accepting a
// string SQLite rejects would drop required tests.
//
// Allowed:
//   * column references that resolve (or `NEW.`/`OLD.` columns in a trigger WHEN);
//   * literals: decimal integers (at most 18 digits), reals, strings, NULL, TRUE,
//     FALSE;
//   * parentheses, nested at most MAX_DEPTH deep;
//   * OR, AND, NOT; = == != <> < <= > >=; unary + and -; binary + - * / %;
//   * IS [NOT] <operand>, which covers IS [NOT] NULL;
//   * [NOT] IN (a non-empty list of at most MAX_IN_ITEMS literals);
//   * [NOT] BETWEEN a AND b;
//   * [NOT] LIKE|GLOB '<string literal>' without ESCAPE (the operator form is a
//     plain two-operand test; ESCAPE is a third GLOB argument SQLite rejects);
//   * the scalar functions in FUNCTIONS, with their fixed arities.
// Everything else is outside the proof: CAST, CASE, COLLATE, subqueries, EXISTS,
// bound parameters, `||` and bitwise operators, hexadecimal literals, unknown or
// variable-argument functions, window and aggregate syntax, RAISE outside the exact
// trigger body, and any expression over MAX_TOKENS tokens.
// ---------------------------------------------------------------------------

/// Scalar functions a CHECK, DEFAULT or WHEN may call as `(name, min, max)`
/// arguments. This is the set the shipped store migrations use in their CHECK
/// expressions that the legacy private connection also resolves: `rsi_*` are the two
/// functions the legacy connection registered; the rest are SQLite built-ins that are
/// deterministic. Every arity is fixed or tightly bounded by SQLite itself.
const FUNCTIONS: &[(&str, usize, usize)] = &[
    ("rsi_uuid_is_canonical", 1, 1),
    ("rsi_rfc3339_nanos_is_canonical", 1, 1),
    ("json_valid", 1, 1),
    ("length", 1, 1),
    ("lower", 1, 1),
    ("trim", 1, 1),
    ("typeof", 1, 1),
    ("instr", 2, 2),
    ("substr", 2, 3),
];

/// Most tokens in one proven expression.
pub(super) const MAX_TOKENS: usize = 256;
/// Deepest nesting of parentheses, function arguments, `NOT`s and prefix signs.
const MAX_DEPTH: usize = 16;
/// Most literals in one `IN (...)` list.
const MAX_IN_ITEMS: usize = 32;
/// Most arguments to one function call.
const MAX_ARGS: usize = 8;
/// Longest integer literal in digits (SQLite reads a longer one as a real).
const MAX_INTEGER_DIGITS: usize = 18;
/// Longest numeric literal in bytes.
const MAX_NUMBER_BYTES: usize = 30;

/// A decimal integer or real literal, strictly: no hexadecimal form, no glued
/// identifier characters, and bounded in size.
pub(super) fn strict_number(word: &str) -> bool {
    let b = word.as_bytes();
    if !number_like(word) || b.len() > MAX_NUMBER_BYTES {
        return false;
    }
    if b.len() > 1 && b[0] == b'0' && matches!(b[1], b'x' | b'X') {
        return false;
    }
    !b.iter().all(u8::is_ascii_digit) || b.len() <= MAX_INTEGER_DIGITS
}

struct Parser<'a> {
    toks: &'a [Tok],
    at: usize,
    depth: usize,
    columns: &'a BTreeSet<String>,
    row_refs: bool,
}

type Parsed = Option<()>;

impl Parser<'_> {
    fn peek(&self, ahead: usize) -> Option<&Tok> {
        self.toks.get(self.at + ahead)
    }

    fn word(&self, ahead: usize, word: &str) -> bool {
        self.peek(ahead).is_some_and(|t| is_word(t, word))
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let hit = self.word(0, word);
        self.at += usize::from(hit);
        hit
    }

    fn eat_punct(&mut self, c: char) -> bool {
        let hit = self.peek(0) == Some(&Tok::Punct(c));
        self.at += usize::from(hit);
        hit
    }

    /// Consume a single-character operator from `set` or a two-character one from `ops`.
    fn eat_operator(&mut self, set: &str, ops: &[&str]) -> bool {
        let hit = match self.peek(0) {
            Some(Tok::Punct(c)) => set.contains(*c),
            Some(Tok::Op(o)) => ops.contains(o),
            _ => false,
        };
        self.at += usize::from(hit);
        hit
    }

    fn nested(&mut self, parse: fn(&mut Self) -> Parsed) -> Parsed {
        self.depth += 1;
        let result = if self.depth <= MAX_DEPTH {
            parse(self)
        } else {
            None
        };
        self.depth -= 1;
        result
    }

    fn or(&mut self) -> Parsed {
        self.nested(Self::or_chain)
    }

    fn or_chain(&mut self) -> Parsed {
        self.and()?;
        while self.eat_word("or") {
            self.and()?;
        }
        Some(())
    }

    fn and(&mut self) -> Parsed {
        self.not()?;
        while self.eat_word("and") {
            self.not()?;
        }
        Some(())
    }

    fn not(&mut self) -> Parsed {
        if self.eat_word("not") {
            self.nested(Self::not)
        } else {
            self.equality()
        }
    }

    fn equality(&mut self) -> Parsed {
        self.compare()?;
        loop {
            if self.eat_operator("=", &["==", "!=", "<>"]) {
                self.compare()?;
            } else if self.eat_word("is") {
                self.eat_word("not");
                self.compare()?;
            } else {
                let negated = self.word(0, "not")
                    && ["in", "like", "glob", "between"]
                        .iter()
                        .any(|w| self.word(1, w));
                self.at += usize::from(negated);
                if self.eat_word("in") {
                    self.in_list()?;
                } else if self.eat_word("like") || self.eat_word("glob") {
                    // The pattern is one string literal; `ESCAPE` is left unconsumed,
                    // so the expression no longer parses in full and is not proven.
                    if self.peek(0) != Some(&Tok::Str) {
                        return None;
                    }
                    self.at += 1;
                } else if self.eat_word("between") {
                    self.additive()?;
                    if !self.eat_word("and") {
                        return None;
                    }
                    self.additive()?;
                } else {
                    return Some(());
                }
            }
        }
    }

    /// `( literal {, literal} )`: empty and over-long lists are not proven.
    fn in_list(&mut self) -> Parsed {
        if !self.eat_punct('(') {
            return None;
        }
        let mut items = 0;
        loop {
            self.signed_literal()?;
            items += 1;
            if items > MAX_IN_ITEMS {
                return None;
            }
            if !self.eat_punct(',') {
                break;
            }
        }
        self.eat_punct(')').then_some(())
    }

    fn signed_literal(&mut self) -> Parsed {
        if self.peek(0) == Some(&Tok::Str) {
            self.at += 1;
            return Some(());
        }
        let signed = self.eat_operator("+-", &[]);
        match self.peek(0)? {
            Tok::Word(w)
                if strict_number(w)
                    || (!signed && matches!(w.as_str(), "null" | "true" | "false")) =>
            {
                self.at += 1;
                Some(())
            }
            _ => None,
        }
    }

    fn compare(&mut self) -> Parsed {
        self.additive()?;
        while self.eat_operator("<>", &["<=", ">="]) {
            self.additive()?;
        }
        Some(())
    }

    fn additive(&mut self) -> Parsed {
        self.multiplicative()?;
        while self.eat_operator("+-", &[]) {
            self.multiplicative()?;
        }
        Some(())
    }

    fn multiplicative(&mut self) -> Parsed {
        self.unary()?;
        while self.eat_operator("*/%", &[]) {
            self.unary()?;
        }
        Some(())
    }

    fn unary(&mut self) -> Parsed {
        if self.eat_operator("-+", &[]) {
            self.nested(Self::unary)
        } else {
            self.primary()
        }
    }

    fn primary(&mut self) -> Parsed {
        let toks = self.toks;
        match toks.get(self.at)? {
            Tok::Str => self.at += 1,
            Tok::Punct('(') => {
                self.at += 1;
                self.or()?;
                if !self.eat_punct(')') {
                    return None;
                }
            }
            Tok::Ident(name) => {
                // a trigger's unqualified column is "no such column" in SQLite
                if self.row_refs || !self.columns.contains(name) {
                    return None;
                }
                self.at += 1;
            }
            Tok::Word(w) => self.word_primary(w)?,
            Tok::Punct(_) | Tok::Op(_) => return None,
        }
        Some(())
    }

    fn word_primary(&mut self, w: &str) -> Parsed {
        if strict_number(w) || matches!(w, "null" | "true" | "false") {
            self.at += 1;
        } else if self.row_refs
            && (w == "new" || w == "old")
            && self.peek(1) == Some(&Tok::Punct('.'))
        {
            let column = self.peek(2).and_then(plain_name)?;
            if !self.columns.contains(column) {
                return None;
            }
            self.at += 3;
        } else if self.peek(1) == Some(&Tok::Punct('(')) {
            let &(_, low, high) = FUNCTIONS.iter().find(|f| f.0 == w)?;
            self.at += 2;
            let mut count = 0;
            loop {
                self.or()?;
                count += 1;
                if count > MAX_ARGS || !self.eat_punct(',') {
                    break;
                }
            }
            if !self.eat_punct(')') || !(low..=high).contains(&count) {
                return None;
            }
        } else if !self.row_refs
            && self.peek(0).and_then(plain_name).is_some()
            && self.columns.contains(w)
        {
            self.at += 1;
        } else {
            return None;
        }
        Some(())
    }
}

/// A scalar expression, parsed in full and inside the allowlist above, whose every
/// name resolves: columns of `columns` (or, when `row_refs`, only `NEW.`/`OLD.`
/// columns). Any token sequence that is not exactly one allowlisted expression is
/// outside the proof.
pub(super) fn expr_ok(toks: &[Tok], columns: &BTreeSet<String>, row_refs: bool) -> bool {
    if toks.is_empty() || toks.len() > MAX_TOKENS {
        return false;
    }
    let mut parser = Parser {
        toks,
        at: 0,
        depth: 0,
        columns,
        row_refs,
    };
    parser.or().is_some() && parser.at == toks.len()
}
