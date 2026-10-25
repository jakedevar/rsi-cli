//! Migration-specific selection (#1341, #1618, #2029, #1285): rewind-only fixture
//! edits, additive/isolated migrations, rewind-caller scans, migration catalog
//! consumers and the numbering/pending-predecessor audit. The released-migration
//! policy itself (pins, declarations, renumbering) is #361's
//! `released_migrations`; this module only reads its public parsers.
use super::{
    diffseq::{self, Tag},
    filters::Observer,
    identity::ItemReferences,
    source::{self, SourceTree, lib_source, rust_functions, rust_functions_ungated},
    sql_expr::{
        Tok, expr_ok, is_word, name_of, number_like, plain_name, sql_tokens, strict_number,
    },
};
use crate::{
    re::{Re, group},
    released_migrations,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

pub const REWIND_FIXTURE: &str = "crates/rsid-store/src/store/tests.rs";
pub const REWIND_HELPERS: [&str; 3] = [
    "rewind_post_v121_tail_to",
    "assert_fixture_matches_claimed_version",
    "assert_post_v77_chain_replayed",
];
pub const MIGRATIONS: &str = "crates/rsid-store/src/store/migrations/";
pub const PRE_SPLIT_MIGRATIONS: &str = "crates/rsid/src/store/migrations/";
const ALIGN_BUDGET: usize = 50_000_000;

static MIGRATION_FILE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^crates/rsid-store/src/store/migrations/v(\d+)\.rs$"));
static PRE_SPLIT_FILE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"^crates/rsid/src/store/migrations/v(\d+)\.rs$"));
static ADDITIVE_SQL: LazyLock<Re> = LazyLock::new(|| {
    Re::new(r"(?i)add_column_if_not_exists(?:_tx)?\s*\(|\bCREATE\s+(?:UNIQUE\s+)?INDEX\b")
});
static DESTRUCTIVE_SQL: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"(?i)\b(?:DROP|DELETE|UPDATE|INSERT|REPLACE|RENAME|TRIGGER|VIEW|SELECT|CREATE\s+TABLE|CREATE\s+VIRTUAL|execute_batch|table_rebuild\w*|rebuild_\w+)\b",
    )
});
static CHAIN_PINS: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        r"every_recovered_migration_step_actually_executes|_fixture_rewinds|previous_schema_upgrades_",
    )
});
static TEST_ATTR: LazyLock<Re> = LazyLock::new(|| Re::new(r"#\[(?:[a-z_]+::)?test\b"));
static CALLS: LazyLock<Re> = LazyLock::new(|| Re::new(r"\b([A-Za-z_]\w*)\s*\("));
static WORD: LazyLock<Re> = LazyLock::new(|| Re::new(r"[A-Za-z_]\w*"));

pub fn migration_file(path: &str) -> Option<i64> {
    MIGRATION_FILE
        .search(path)
        .or_else(|| PRE_SPLIT_FILE.search(path))
        .and_then(|c| group(&c, 1).parse().ok())
}

/// Whether every change of the rewind fixture lies inside a rewind helper or
/// the teardown-coverage constant (#1341).
pub fn rewind_only_change(before: &str, after: &str) -> bool {
    fn covered(text: &str) -> BTreeSet<usize> {
        let mut result = BTreeSet::new();
        for f in rust_functions_ungated(text) {
            if REWIND_HELPERS.contains(&f.name.as_str()) {
                result.extend(f.start..f.end);
            }
        }
        let constant = Re::new(r"^const REWIND_TEARDOWN_COVERED_THROUGH: i32 = \d+;$");
        for (i, line) in text.lines().enumerate() {
            if line.contains("REWIND_TEARDOWN_COVERED_THROUGH") && constant.is_match(line) {
                result.insert(i);
            }
        }
        result
    }
    let old: Vec<String> = before.lines().map(str::to_owned).collect();
    let new: Vec<String> = after.lines().map(str::to_owned).collect();
    let (old_allowed, new_allowed) = (covered(before), covered(after));
    // Trim the identical prefix/suffix: migration maintenance changes a short tail.
    let mut first = 0;
    let (mut old_end, mut new_end) = (old.len(), new.len());
    while first < old_end.min(new_end) && old[first] == new[first] {
        first += 1;
    }
    while old_end > first && new_end > first && old[old_end - 1] == new[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    // A row with more occurrences on one side leaves some occurrence in a
    // changed region of any alignment: if none of them is covered, the change
    // is not rewind-only and the quadratic matching can be skipped.
    let mut imbalance = HashMap::<&str, i64>::new();
    for row in &old[first..old_end] {
        *imbalance.entry(row).or_default() += 1;
    }
    for row in &new[first..new_end] {
        *imbalance.entry(row).or_default() -= 1;
    }
    let excess: BTreeSet<&str> = imbalance
        .iter()
        .filter(|(_, n)| **n != 0)
        .map(|(r, _)| *r)
        .collect();
    let mut rows: BTreeSet<&str> = BTreeSet::new();
    for i in first..old_end {
        if old_allowed.contains(&i) && imbalance.get(old[i].as_str()).is_some_and(|n| *n > 0) {
            rows.insert(&old[i]);
        }
    }
    for i in first..new_end {
        if new_allowed.contains(&i) && imbalance.get(new[i].as_str()).is_some_and(|n| *n < 0) {
            rows.insert(&new[i]);
        }
    }
    if excess.difference(&rows).next().is_some() {
        return false;
    }
    let Some(codes) = diffseq::opcodes(&old[first..old_end], &new[first..new_end], ALIGN_BUDGET)
    else {
        return false;
    };
    let changes: Vec<_> = codes.into_iter().filter(|c| c.0 != Tag::Equal).collect();
    !changes.is_empty()
        && changes.iter().all(|&(_, i, j, k, l)| {
            (first + i..first + j).all(|n| old_allowed.contains(&n))
                && (first + k..first + l).all(|n| new_allowed.contains(&n))
        })
}

// ---------------------------------------------------------------- SQL proof

/// Complete SQLite statements as `(start, end)` spans, retaining a trigger's
/// BEGIN/END body. `None` when anything remains after the last statement.
fn statements(
    sql: &str,
    tokens: &[(Tok, usize, usize)],
) -> Option<Vec<(usize, usize, Vec<usize>)>> {
    let mut found = Vec::new();
    let mut from = 0;
    let mut indices: Vec<usize> = Vec::new();
    for (n, (tok, _, stop)) in tokens.iter().enumerate() {
        indices.push(n);
        if *tok != Tok::Punct(';') {
            continue;
        }
        let words: Vec<&str> = indices
            .iter()
            .filter_map(|i| match &tokens[*i].0 {
                Tok::Word(w) => Some(w.as_str()),
                _ => None,
            })
            .take(3)
            .collect();
        let trigger = words.first() == Some(&"create")
            && (words.get(1) == Some(&"trigger")
                || (matches!(words.get(1), Some(&"temp") | Some(&"temporary"))
                    && words.get(2) == Some(&"trigger")));
        let complete = !trigger
            || (indices.len() >= 2 && is_word(&tokens[indices[indices.len() - 2]].0, "end"));
        if complete {
            found.push((from, *stop, std::mem::take(&mut indices)));
            from = *stop;
        }
    }
    (sql[from..].trim().is_empty()).then_some(found)
}

fn paren_balanced(tokens: &[Tok]) -> bool {
    let mut depth = 0i32;
    for t in tokens {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => (),
        }
    }
    depth == 0
}

// -------------------------------------------------- strict DDL grammar (#457)
//
// The legacy proof executed the literal batch in a private SQLite connection, so
// SQLite itself rejected an index on a missing column, a duplicate object, a CHECK
// naming an unknown column or function and the like. This port has no embedded
// SQLite, so it accepts only a small grammar whose every name it resolves itself;
// any other shape is "not proven isolated" and retains every rewind caller.

/// Index of the `)` closing the `(` at `open`.
fn matching_close(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => (),
        }
    }
    None
}

/// The pieces between top-level commas; an empty piece is a syntax error.
fn split_top(toks: &[Tok]) -> Option<Vec<&[Tok]>> {
    let (mut parts, mut depth, mut from) = (Vec::new(), 0i32, 0);
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth -= 1,
            Tok::Punct(',') if depth == 0 => {
                parts.push(&toks[from..i]);
                from = i + 1;
            }
            _ => (),
        }
    }
    parts.push(&toks[from..]);
    parts.iter().all(|p| !p.is_empty()).then_some(parts)
}

fn conflict_clause(toks: &[Tok], at: &mut usize) -> bool {
    if toks.get(*at).is_some_and(|t| is_word(t, "on"))
        && toks.get(*at + 1).is_some_and(|t| is_word(t, "conflict"))
    {
        let ok = toks.get(*at + 2).is_some_and(|t| {
            ["rollback", "abort", "fail", "ignore", "replace"]
                .iter()
                .any(|w| is_word(t, w))
        });
        *at += 3;
        return ok;
    }
    true
}

/// `( column [COLLATE c] [ASC|DESC], ... )` over declared columns only.
fn column_list(toks: &[Tok], open: usize, columns: &BTreeSet<String>) -> Option<(usize, usize)> {
    if toks.get(open) != Some(&Tok::Punct('(')) {
        return None;
    }
    let close = matching_close(toks, open)?;
    let parts = split_top(&toks[open + 1..close])?;
    // Repeated occurrences count: SQLite refuses an index past its column limit.
    if parts.len() > MAX_LIST_ITEMS {
        return None;
    }
    for part in &parts {
        if !columns.contains(name_of(&part[0])?) {
            return None;
        }
        let mut at = 1;
        if part.get(at).is_some_and(|t| is_word(t, "collate")) {
            let c = part.get(at + 1)?;
            if !["binary", "nocase", "rtrim"].iter().any(|w| is_word(c, w)) {
                return None;
            }
            at += 2;
        }
        if part
            .get(at)
            .is_some_and(|t| is_word(t, "asc") || is_word(t, "desc"))
        {
            at += 1;
        }
        if at != part.len() {
            return None;
        }
    }
    Some((close, parts.len()))
}

/// `REFERENCES parent [(cols)] [ON DELETE|UPDATE action] [[NOT] DEFERRABLE ...]`;
/// `child_columns` is the foreign key's column count, which SQLite checks against
/// an explicit parent list when the table is created.
fn references_clause(toks: &[Tok], at: &mut usize, child_columns: usize) -> bool {
    *at += 1; // REFERENCES
    if toks.get(*at).and_then(plain_name).is_none() {
        return false;
    }
    *at += 1;
    if toks.get(*at) == Some(&Tok::Punct('(')) {
        let Some(close) = matching_close(toks, *at) else {
            return false;
        };
        let Some(parts) = split_top(&toks[*at + 1..close]) else {
            return false;
        };
        if parts.len() != child_columns
            || parts.len() > MAX_LIST_ITEMS
            || parts
                .iter()
                .any(|p| p.len() != 1 || plain_name(&p[0]).is_none())
        {
            return false;
        }
        *at = close + 1;
    }
    let mut clauses = 0usize;
    loop {
        clauses += 1;
        // the pass that finds no more clauses is counted too
        if clauses > MAX_REFERENCE_CLAUSES + 1 {
            return false;
        }
        match toks.get(*at) {
            Some(t) if is_word(t, "on") => {
                let event = toks
                    .get(*at + 1)
                    .is_some_and(|t| is_word(t, "delete") || is_word(t, "update"));
                if !event {
                    return false;
                }
                *at += 2;
                let action = match toks.get(*at) {
                    Some(Tok::Word(w)) => Some(w.as_str()),
                    _ => None,
                };
                match action {
                    Some("cascade") | Some("restrict") => *at += 1,
                    Some("set")
                        if toks
                            .get(*at + 1)
                            .is_some_and(|t| is_word(t, "null") || is_word(t, "default")) =>
                    {
                        *at += 2
                    }
                    Some("no") if toks.get(*at + 1).is_some_and(|t| is_word(t, "action")) => {
                        *at += 2
                    }
                    _ => return false,
                }
            }
            Some(t) if is_word(t, "not") || is_word(t, "deferrable") => {
                if is_word(t, "not") {
                    *at += 1;
                }
                if !toks.get(*at).is_some_and(|t| is_word(t, "deferrable")) {
                    return false;
                }
                *at += 1;
                if toks.get(*at).is_some_and(|t| is_word(t, "initially")) {
                    let ok = toks
                        .get(*at + 1)
                        .is_some_and(|t| is_word(t, "deferred") || is_word(t, "immediate"));
                    if !ok {
                        return false;
                    }
                    *at += 2;
                }
            }
            _ => return true,
        }
    }
}

/// Whether `tokens` (after a column's type) is a valid constraint sequence.
/// `primary` counts PRIMARY KEY clauses seen.
fn column_constraints(
    toks: &[Tok],
    columns: &BTreeSet<String>,
    primary: &mut usize,
    integer_pk: bool,
    rowid: bool,
) -> bool {
    let mut at = 0;
    let mut clauses = 0usize;
    while at < toks.len() {
        clauses += 1;
        if clauses > MAX_COLUMN_CONSTRAINTS {
            return false;
        }
        let t = &toks[at];
        if is_word(t, "constraint") {
            if toks.get(at + 1).and_then(plain_name).is_none() {
                return false;
            }
            at += 2;
        } else if is_word(t, "primary") {
            if !toks.get(at + 1).is_some_and(|t| is_word(t, "key")) {
                return false;
            }
            *primary += 1;
            at += 2;
            if toks
                .get(at)
                .is_some_and(|t| is_word(t, "asc") || is_word(t, "desc"))
            {
                at += 1;
            }
            if !conflict_clause(toks, &mut at) {
                return false;
            }
            if toks.get(at).is_some_and(|t| is_word(t, "autoincrement")) {
                if !(integer_pk && rowid) {
                    return false;
                }
                at += 1;
            }
        } else if is_word(t, "not") {
            if !toks.get(at + 1).is_some_and(|t| is_word(t, "null")) {
                return false;
            }
            at += 2;
            if !conflict_clause(toks, &mut at) {
                return false;
            }
        } else if is_word(t, "null") {
            at += 1;
            if !conflict_clause(toks, &mut at) {
                return false;
            }
        } else if is_word(t, "unique") {
            at += 1;
            if !conflict_clause(toks, &mut at) {
                return false;
            }
        } else if is_word(t, "check") {
            if toks.get(at + 1) != Some(&Tok::Punct('(')) {
                return false;
            }
            let Some(close) = matching_close(toks, at + 1) else {
                return false;
            };
            if !expr_ok(&toks[at + 2..close], columns, false) {
                return false;
            }
            at = close + 1;
        } else if is_word(t, "default") {
            at += 1;
            match toks.get(at) {
                Some(Tok::Str) => at += 1,
                Some(Tok::Word(w))
                    if strict_number(w) || ["null", "true", "false"].contains(&w.as_str()) =>
                {
                    at += 1
                }
                Some(Tok::Punct('+' | '-')) if matches!(toks.get(at + 1), Some(Tok::Word(w)) if strict_number(w)) => {
                    at += 2
                }
                Some(Tok::Punct('(')) => {
                    let Some(close) = matching_close(toks, at) else {
                        return false;
                    };
                    // a default is a constant expression: no column may appear
                    if !expr_ok(&toks[at + 1..close], &BTreeSet::new(), false) {
                        return false;
                    }
                    at = close + 1;
                }
                _ => return false,
            }
        } else if is_word(t, "collate") {
            let ok = toks
                .get(at + 1)
                .is_some_and(|c| ["binary", "nocase", "rtrim"].iter().any(|w| is_word(c, w)));
            if !ok {
                return false;
            }
            at += 2;
        } else if is_word(t, "references") {
            if !references_clause(toks, &mut at, 1) {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}

const CONSTRAINT_STARTS: &[&str] = &[
    "constraint",
    "primary",
    "not",
    "null",
    "unique",
    "check",
    "default",
    "collate",
    "references",
    "generated",
    "as",
];

/// A table's declared columns when the whole `CREATE TABLE` body is inside the
/// grammar above and every name in it resolves.
fn table_columns(body: &[Tok], without_rowid: bool) -> Option<BTreeSet<String>> {
    let items = split_top(body)?;
    let constraint_item = |item: &[Tok]| {
        ["constraint", "primary", "unique", "check", "foreign"]
            .iter()
            .any(|w| is_word(&item[0], w))
    };
    let mut columns = BTreeSet::new();
    for item in items.iter().filter(|i| !constraint_item(i)) {
        let name = plain_name(&item[0])?;
        if name.starts_with("sqlite_") || !columns.insert(name.to_owned()) {
            return None;
        }
    }
    if columns.is_empty()
        || columns.len() > MAX_TABLE_COLUMNS
        || items.len() - columns.len() > MAX_TABLE_CONSTRAINTS
    {
        return None;
    }
    let mut primary = 0usize;
    for item in &items {
        if constraint_item(item) {
            let mut at = 0;
            if is_word(&item[0], "constraint") {
                item.get(1).and_then(plain_name)?;
                at = 2;
            }
            let head = item.get(at)?;
            if is_word(head, "primary") || is_word(head, "unique") {
                if is_word(head, "primary") {
                    if !item.get(at + 1).is_some_and(|t| is_word(t, "key")) {
                        return None;
                    }
                    primary += 1;
                    at += 1;
                }
                let (close, _) = column_list(item, at + 1, &columns)?;
                at = close + 1;
                if !conflict_clause(item, &mut at) || at != item.len() {
                    return None;
                }
            } else if is_word(head, "check") {
                if item.get(at + 1) != Some(&Tok::Punct('(')) {
                    return None;
                }
                let close = matching_close(item, at + 1)?;
                if close + 1 != item.len() || !expr_ok(&item[at + 2..close], &columns, false) {
                    return None;
                }
            } else if is_word(head, "foreign") {
                if !item.get(at + 1).is_some_and(|t| is_word(t, "key")) {
                    return None;
                }
                let (close, n) = column_list(item, at + 2, &columns)?;
                at = close + 1;
                if !item.get(at).is_some_and(|t| is_word(t, "references")) {
                    return None;
                }
                if !references_clause(item, &mut at, n) || at != item.len() {
                    return None;
                }
            } else {
                return None;
            }
        } else {
            // name, optional type words and `(n[,m])`, then constraints
            let mut at = 1;
            let mut type_words = Vec::new();
            let mut sized = false;
            while let Some(t) = item.get(at) {
                if CONSTRAINT_STARTS.iter().any(|w| is_word(t, w)) {
                    break;
                }
                if is_word(t, "generated") {
                    return None;
                }
                match t {
                    // a type name is plain words, then at most one `(n[,m])`
                    Tok::Word(w) if !sized && plain_name(t).is_some() && !number_like(w) => {
                        type_words.push(w.as_str());
                        if type_words.len() > MAX_TYPE_WORDS {
                            return None;
                        }
                    }
                    Tok::Punct('(') if !sized && !type_words.is_empty() => {
                        sized = true;
                        let close = matching_close(item, at)?;
                        let sizes = split_top(&item[at + 1..close])?;
                        let numeric = sizes.iter().all(|p| {
                            matches!(p, [Tok::Word(w)] if strict_number(w))
                                || matches!(p, [Tok::Punct('+' | '-'), Tok::Word(w)] if strict_number(w))
                        });
                        if !numeric || sizes.len() > 2 {
                            return None;
                        }
                        at = close;
                    }
                    _ => return None,
                }
                at += 1;
            }
            let integer_pk = type_words == ["integer"];
            if !column_constraints(
                &item[at..],
                &columns,
                &mut primary,
                integer_pk,
                !without_rowid,
            ) {
                return None;
            }
        }
    }
    (primary <= 1 && (!without_rowid || primary == 1)).then_some(columns)
}

/// An object name at `at`, with an optional `main.` schema qualifier (a create
/// target only: SQLite takes the table an index or trigger sits on unqualified).
fn qualified_name(toks: &[Tok], at: usize) -> Option<(String, usize)> {
    let first = plain_name(toks.get(at)?)?;
    if toks.get(at + 1) == Some(&Tok::Punct('.')) {
        if first != "main" {
            return None;
        }
        return Some((plain_name(toks.get(at + 2)?)?.to_owned(), at + 3));
    }
    Some((first.to_owned(), at + 1))
}

/// The position after an optional `IF NOT EXISTS` at `at`; `None` for any other `IF`.
fn if_not_exists(toks: &[Tok], at: usize) -> Option<usize> {
    if !toks.get(at).is_some_and(|t| is_word(t, "if")) {
        return Some(at);
    }
    let spelled = toks.get(at + 1).is_some_and(|t| is_word(t, "not"))
        && toks.get(at + 2).is_some_and(|t| is_word(t, "exists"));
    spelled.then_some(at + 3)
}

/// `BEGIN SELECT RAISE ( ABORT , 'message' ) ; END ;` and nothing else.
fn raise_only_body(toks: &[Tok]) -> bool {
    matches!(
        toks,
        [begin, select, raise, Tok::Punct('('), abort, Tok::Punct(','), Tok::Str, Tok::Punct(')'), Tok::Punct(';'), end, Tok::Punct(';')]
            if is_word(begin, "begin")
                && is_word(select, "select")
                && is_word(raise, "raise")
                && is_word(abort, "abort")
                && is_word(end, "end")
    )
}

/// Bounds well inside SQLite's own limits (2000 columns per table or index, 1000-deep
/// expression trees, 1000 function arguments), so no statement the proof accepts can
/// hit one. EVERY repeatable construct the DDL proof accepts has a bound here or in
/// `sql_expr` (#462): a list SQLite rejects only past a limit must never be proven.
/// The shipped isolated migrations use at most 3 columns, 1 table constraint, 2-item
/// lists, 4 clauses on a column and 5 statements.
const MAX_SQL_TOKENS: usize = 20_000;
const MAX_STATEMENTS: usize = 64;
const MAX_TABLE_COLUMNS: usize = 64;
const MAX_TABLE_CONSTRAINTS: usize = 64;
/// Items in one column list: CREATE INDEX, table PRIMARY KEY or UNIQUE, FOREIGN KEY
/// and its parent list. Repeating a column counts every occurrence.
const MAX_LIST_ITEMS: usize = 16;
/// Constraint clauses after one column definition.
const MAX_COLUMN_CONSTRAINTS: usize = 16;
/// Words in one column type name.
const MAX_TYPE_WORDS: usize = 4;
/// `ON ...` and `DEFERRABLE` clauses after one REFERENCES.
const MAX_REFERENCE_CLAUSES: usize = 8;

/// Conservative stand-in for the authorizer over a private in-memory SQLite:
/// literal DDL may create new tables, indexes on those tables and triggers on
/// those tables that only `RAISE`; every other shape is not proven isolated.
fn sql_isolated(sql: &str) -> bool {
    if sql.contains('\0') {
        return false;
    }
    let Some(tokens) = sql_tokens(sql) else {
        return false;
    };
    if tokens.len() > MAX_SQL_TOKENS {
        return false;
    }
    let Some(spans) = statements(sql, &tokens) else {
        return false;
    };
    if spans.is_empty() || spans.len() > MAX_STATEMENTS {
        return false;
    }
    let start_re = Re::new(r"(?i)^CREATE\s+(?:TABLE|(?:UNIQUE\s+)?INDEX|TRIGGER)\b");
    let table_if = Re::new(r"(?i)^CREATE\s+TABLE\s+IF\b");
    let comments = Re::new(r"(?s)/\*.*?\*/|--[^\n]*");
    let mut tables = BTreeMap::<String, BTreeSet<String>>::new();
    let mut relations = BTreeSet::<String>::new();
    let mut triggers = BTreeSet::<String>::new();
    for (from, stop, indices) in spans {
        let statement = sql[from..stop].trim();
        if !start_re.is_match(statement) {
            return false;
        }
        let uncommented = comments.sub(statement, " ").into_owned();
        if table_if.is_match(&uncommented) {
            return false;
        }
        let owned: Vec<Tok> = indices.iter().map(|i| tokens[*i].0.clone()).collect();
        if !paren_balanced(&owned) {
            return false;
        }
        let mut at = 1; // after CREATE
        let kind = &owned[at];
        // Tables and indexes share one namespace in SQLite; a repeat fails there.
        let mut declare = |name: &str| -> bool {
            !name.starts_with("sqlite_") && relations.insert(name.to_owned())
        };
        if is_word(kind, "table") {
            at += 1;
            let Some((name, next)) = qualified_name(&owned, at) else {
                return false;
            };
            if owned.get(next) != Some(&Tok::Punct('(')) {
                return false;
            }
            let Some(close) = matching_close(&owned, next) else {
                return false;
            };
            // After the body only `WITHOUT ROWID` (and the statement's `;`) may follow.
            let tail: Vec<&Tok> = owned[close + 1..]
                .iter()
                .filter(|t| **t != Tok::Punct(';'))
                .collect();
            let without_rowid = match tail.as_slice() {
                [] => false,
                [a, b] if is_word(a, "without") && is_word(b, "rowid") => true,
                _ => return false,
            };
            let Some(columns) = table_columns(&owned[next + 1..close], without_rowid) else {
                return false;
            };
            if !declare(&name) {
                return false;
            }
            tables.insert(name, columns);
        } else if is_word(kind, "unique") || is_word(kind, "index") {
            if is_word(kind, "unique") {
                if !owned.get(at + 1).is_some_and(|t| is_word(t, "index")) {
                    return false;
                }
                at += 2;
            } else {
                at += 1;
            }
            let Some(at) = if_not_exists(&owned, at) else {
                return false;
            };
            let Some((index, next)) = qualified_name(&owned, at) else {
                return false;
            };
            if !owned.get(next).is_some_and(|t| is_word(t, "on")) {
                return false;
            }
            let Some(table) = owned.get(next + 1).and_then(plain_name) else {
                return false;
            };
            let Some(columns) = tables.get(table) else {
                return false;
            };
            // Every indexed column must be a declared plain column of the table
            // (no expression, partial `WHERE` or trailing clause), and nothing may follow.
            let Some((close, _)) = column_list(&owned, next + 2, columns) else {
                return false;
            };
            if owned[close + 1..].iter().any(|t| *t != Tok::Punct(';')) {
                return false;
            }
            if !declare(&index) {
                return false;
            }
        } else if is_word(kind, "trigger") {
            let Some(at) = if_not_exists(&owned, at + 1) else {
                return false;
            };
            let Some((trigger, next)) = qualified_name(&owned, at) else {
                return false;
            };
            let Some(on) = (next..owned.len()).find(|i| is_word(&owned[*i], "on")) else {
                return false;
            };
            // [BEFORE|AFTER] DELETE|INSERT|UPDATE: no `INSTEAD OF`, no `UPDATE OF cols`.
            let event: Vec<&Tok> = owned[next..on].iter().collect();
            let timing = event
                .first()
                .is_some_and(|t| is_word(t, "before") || is_word(t, "after"));
            let events = &event[usize::from(timing)..];
            let named = events.len() == 1
                && ["delete", "insert", "update"]
                    .iter()
                    .any(|w| is_word(events[0], w));
            if !named {
                return false;
            }
            let Some(table) = owned.get(on + 1).and_then(plain_name) else {
                return false;
            };
            let Some(columns) = tables.get(table) else {
                return false;
            };
            let mut cursor = on + 2;
            if owned.get(cursor).is_some_and(|t| is_word(t, "for")) {
                let each = owned.get(cursor + 1).is_some_and(|t| is_word(t, "each"))
                    && owned.get(cursor + 2).is_some_and(|t| is_word(t, "row"));
                if !each {
                    return false;
                }
                cursor += 3;
            }
            if owned.get(cursor).is_some_and(|t| is_word(t, "when")) {
                let Some(begin) = (cursor + 1..owned.len()).find(|i| is_word(&owned[*i], "begin"))
                else {
                    return false;
                };
                if !expr_ok(&owned[cursor + 1..begin], columns, true) {
                    return false;
                }
                cursor = begin;
            }
            // Only the body's single `SELECT RAISE` may read anything.
            if !raise_only_body(&owned[cursor..]) || !triggers.insert(trigger) {
                return false;
            }
        } else {
            return false;
        }
    }
    !tables.is_empty()
}

/// JSON-style decoding of a Rust string body (`json.loads(.., strict=False)`).
fn decode_literal(body: &str) -> Option<String> {
    let mut escaped = String::with_capacity(body.len() + 2);
    escaped.push('"');
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let next = chars.next()?;
                if (next as u32) < 0x20 {
                    return None;
                }
                escaped.push('\\');
                escaped.push(next);
            }
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    serde_json::from_str::<String>(&escaped).ok()
}

/// Prove literal DDL owns only new tables; ambiguity retains all rewind callers.
pub fn isolated_table_migration(code: &str) -> bool {
    let batch = Re::new(r#"(?s)\btx\.execute_batch\(\s*"((?:\\.|[^"\\])*)"\s*,?\s*\)"#);
    let matches = batch.find_all(code);
    if matches.is_empty() {
        return false;
    }
    let remainder = batch.sub(code, "").into_owned();
    // The shipped journal migrations import the store/result and SQLite
    // transaction types. Imports execute no code; accept only these known
    // bindings so a lookalike transaction helper cannot enter the proof.
    let remainder = Re::new(
        r"\buse\s+(?:super::(?:super::)?Store|crate::error::Result|rusqlite::(?:Transaction|TransactionBehavior|\{\s*Transaction\s*,\s*TransactionBehavior\s*\}))\s*;",
    )
    .sub(&remainder, "")
    .into_owned();
    let remainder = Re::new(r"\bpub(?:\((?:super|crate)\))?\s+")
        .sub(&remainder, "")
        .into_owned();
    let remainder = Re::new(r#"\btx\.pragma_update\(None,\s*"user_version",\s*\d+\)"#)
        .sub(&remainder, "")
        .into_owned();
    let skeleton: BTreeSet<&str> = "impl Store fn self version i32 Result if let tx rusqlite Transaction new_unchecked conn TransactionBehavior Immediate commit Ok"
        .split(' ')
        .collect();
    let migrate = Re::new(r"^migrate_v\d+$");
    for w in WORD.find_all(&remainder) {
        let word = group(&w, 0);
        if !skeleton.contains(word) && !migrate.is_match(word) {
            return false;
        }
    }
    if Re::new(r"\b\w+!\s*[({\[]").is_match(&remainder) {
        return false;
    }
    let allowed =
        Re::new(r"^(?:migrate_v\d+|(?:rusqlite::)?Transaction::new_unchecked|tx\.commit|Ok)$");
    for call in Re::new(r"\b([A-Za-z_]\w*(?:(?:\.|::)[A-Za-z_]\w*)*)\s*\(").find_all(&remainder) {
        if !allowed.is_match(group(&call, 1)) {
            return false;
        }
    }
    let mut sql = Vec::new();
    for m in &matches {
        let Some(decoded) = decode_literal(group(m, 1)) else {
            return false;
        };
        sql.push(decoded);
    }
    sql_isolated(&sql.join("\n"))
}

/// The migration's Rust code with every comment replaced by a space (newlines kept).
pub fn code_without_comments(text: &str) -> Option<String> {
    let lexemes = source::rust_lexemes(text).ok()?;
    let mut code = String::new();
    let mut previous = 0;
    for lex in lexemes
        .iter()
        .filter(|l| l.kind == source::LexemeKind::Comment)
    {
        code.push_str(&text[previous..lex.start]);
        code.push_str(&"\n".repeat(text[lex.start..lex.end].matches('\n').count()));
        code.push(' ');
        previous = lex.end;
    }
    code.push_str(&text[previous..]);
    Some(code)
}

/// True for column/index additions or provably isolated new-table DDL.
pub fn additive_migration(text: &str) -> bool {
    let Some(code) = code_without_comments(text) else {
        return false;
    };
    (ADDITIVE_SQL.is_match(&code) && !DESTRUCTIVE_SQL.is_match(&code))
        || isolated_table_migration(&code)
}

/// Scan direct and indirect callers of the rewind helpers, including fixtures in
/// other modules. `pins_only` keeps only the migration-chain pins (#1618);
/// `catalog_pins` keeps the pins plus every caller comparing `sqlite_master`
/// across the replay (#2029).
pub fn rewind_test_filters(
    sources: &SourceTree,
    pins_only: bool,
    catalog_pins: bool,
) -> Vec<String> {
    struct Row {
        name: String,
        attrs: String,
        calls: BTreeSet<String>,
        catalog: bool,
    }
    let mut functions = Vec::new();
    for (path, text) in &sources.texts {
        if !lib_source(path) {
            continue;
        }
        for f in rust_functions(text, &sources.host) {
            let mut calls: BTreeSet<String> = CALLS.findall1(&f.body).into_iter().collect();
            calls.remove(&f.name);
            functions.push(Row {
                catalog: f.body.contains("sqlite_master"),
                name: f.name,
                attrs: f.attrs,
                calls,
            });
        }
    }
    let mut callers: BTreeSet<String> = [
        "rewind_store_to_schema_version",
        "rewind_post_v121_tail_to",
        "assert_post_v77_chain_replayed",
        "assert_fixture_matches_claimed_version",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    loop {
        let mut expanded = callers.clone();
        expanded.extend(
            functions
                .iter()
                .filter(|f| !f.calls.is_disjoint(&callers))
                .map(|f| f.name.clone()),
        );
        if expanded == callers {
            break;
        }
        callers = expanded;
    }
    let mut filters = BTreeSet::new();
    for f in &functions {
        if TEST_ATTR.is_match(&f.attrs)
            && (callers.contains(&f.name)
                || f.name.contains("_rewinds")
                || f.name.contains("fixture_guard"))
        {
            if (pins_only || catalog_pins)
                && !CHAIN_PINS.is_match(&f.name)
                && !(catalog_pins && f.catalog)
            {
                continue;
            }
            for shard in source::shards_in(&f.attrs) {
                filters.insert(format!("rsid=shard:{shard}:test({})", f.name));
            }
        }
    }
    filters.into_iter().collect()
}

/// All consumers of the replay catalog's constants and helpers, including large
/// helpers. The closure is complete: required migration coverage is not capped.
pub fn migration_metadata_tests(
    sources: &SourceTree,
    index: &ItemReferences,
    path: &str,
) -> Vec<Observer> {
    let text = sources.texts.get(path).map_or("", String::as_str);
    let mut reached: BTreeSet<String> = rust_functions(text, &sources.host)
        .into_iter()
        .map(|f| f.name)
        .collect();
    reached.extend(Re::new(r"\bconst\s+(\w+)\s*:").findall1(text));
    loop {
        let callers: BTreeSet<_> = index
            .functions
            .iter()
            .filter(|f| !source::is_test(&f.function.attrs) && !f.words.is_disjoint(&reached))
            .map(|f| f.function.name.clone())
            .collect();
        if callers.is_subset(&reached) {
            break;
        }
        reached.extend(callers);
    }
    let mut picked = BTreeSet::new();
    for f in &index.functions {
        if source::is_test(&f.function.attrs)
            && (reached.contains(&f.function.name) || !f.words.is_disjoint(&reached))
        {
            for shard in source::shards_in(&f.function.attrs) {
                picked.insert(Observer {
                    module: f.module.clone(),
                    name: f.function.name.clone(),
                    shard,
                });
            }
        }
    }
    picked.into_iter().collect()
}

/// Numbering and layout facts of a candidate's migrations.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct MigrationReport {
    pub new: Vec<i64>,
    pub base_head: Option<i64>,
    pub numbering_conflict: bool,
    pub pending_predecessors: Vec<i64>,
    pub stray_pre_split: Vec<String>,
}

fn layout_heads(files: &[String]) -> (Vec<i64>, bool) {
    for (pattern, split) in [(&*MIGRATION_FILE, true), (&*PRE_SPLIT_FILE, false)] {
        let found: Vec<i64> = files
            .iter()
            .filter_map(|p| pattern.search(p).and_then(|c| group(&c, 1).parse().ok()))
            .collect();
        if !found.is_empty() {
            return (found, split);
        }
    }
    (Vec::new(), true)
}

/// `base_files`/`head_files` list the revisions' tracked paths; `declarations`
/// are the texts of newly added `tools/provisional-migrations/*.json` files.
pub fn migration_report(
    base_files: &[String],
    head_files: &[String],
    added_files: &BTreeSet<String>,
    declarations: &[String],
) -> MigrationReport {
    let (base_heads, _) = layout_heads(base_files);
    let (head_versions, split) = layout_heads(head_files);
    let split_head = split && !head_versions.is_empty();
    let mut new: Vec<i64> = added_files
        .iter()
        .filter_map(|p| migration_file(p))
        .collect();
    new.sort_unstable();
    let stray_pre_split: Vec<String> = added_files
        .iter()
        .filter(|p| split_head && PRE_SPLIT_FILE.is_match(p))
        .cloned()
        .collect();
    let base_head = base_heads.iter().copied().max();
    // A slice numbered after an in-flight sibling declares it (#1285): the numbers
    // it skips are `pending_predecessors` in a newly added provisional declaration.
    let mut pending_predecessors = Vec::new();
    if let (false, Some(head)) = (new.is_empty(), base_head) {
        if !declarations.is_empty() {
            pending_predecessors =
                released_migrations::parse_pending_predecessors(declarations, head)
                    .unwrap_or_default();
        }
    }
    let mut numbering_conflict = false;
    if let (Some(head), false) = (base_head, new.is_empty()) {
        let mut expected = head + 1;
        while pending_predecessors.contains(&expected) {
            expected += 1;
        }
        numbering_conflict = new[0] != expected;
    }
    MigrationReport {
        new,
        base_head,
        numbering_conflict,
        pending_predecessors,
        stray_pre_split,
    }
}
