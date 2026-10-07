//! Operator decision records as the board reads them (#1428): the question
//! split into a headline, context and options, the asker's recommendation,
//! live-first ordering and age. Pure functions over the daemon's row JSON.
//!
//! The daemon (#1415) returns structured `options` (`label`, `detail?`,
//! `recommended`); those win. For older records without them, options and the
//! recommendation are read from the question's lines (`A) text`, `1. text`, a
//! trailing `Recommend: A` or a `(recommended)` marker). A question without
//! two or more recognisable options simply has none and is answered in free
//! text.
use super::board::{number, text};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// A pending record older than this is stale unless the daemon says otherwise.
pub const STALE_AFTER_SECS: i64 = 7 * 86_400;

/// Where a record sits in the live-first list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Waiting for the operator now.
    NeedsYou,
    /// Answered; delivery to the asking manager is still in flight.
    InFlight,
    /// Pending for longer than [`STALE_AFTER_SECS`], or flagged stale.
    Stale,
    /// Settled (answered, declined, withdrawn, ...) or not answerable here.
    Closed,
}

impl Stage {
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::NeedsYou => "NEEDS YOU",
            Self::InFlight => "SENDING",
            Self::Stale => "STALE",
            Self::Closed => "CLOSED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOption {
    /// The asker's label: `A`, `2`.
    pub label: String,
    pub text: String,
}

impl DecisionOption {
    /// The answer text recorded for this option: label and wording, so the
    /// asking manager reads the choice without re-deriving the label.
    #[must_use]
    pub fn answer(&self) -> String {
        if self.text.is_empty() {
            self.label.clone()
        } else {
            format!("{}: {}", self.label, self.text)
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuestion {
    pub headline: String,
    /// Remaining prose lines (background, reasons), in order.
    pub context: Vec<String>,
    pub options: Vec<DecisionOption>,
    /// Index into `options`.
    pub recommended: Option<usize>,
}

fn label_len(t: &str) -> usize {
    let mut chars = t.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() => {
            1 + usize::from(chars.next().is_some_and(|d| d.is_ascii_digit()))
        }
        Some(c) if c.is_ascii_alphabetic() => 1,
        _ => 0,
    }
}

/// `A) text`, `(A) text`, `B. text`, `- 2: text`, `[C] text`.
fn option_marker(line: &str) -> Option<(String, String)> {
    let t = line
        .trim_start()
        .trim_start_matches(['-', '*', '•'])
        .trim_start();
    let (open, t) = match t.strip_prefix(['(', '[']) {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let n = label_len(t);
    if n == 0 {
        return None;
    }
    let (label, rest) = t.split_at(n);
    let mut rest_chars = rest.chars();
    let sep = rest_chars.next()?;
    let ok_sep = if open {
        matches!(sep, ')' | ']')
    } else {
        matches!(sep, ')' | '.' | ':' | ']')
    };
    if !ok_sep {
        return None;
    }
    let body = rest_chars.as_str();
    if !(body.is_empty() || body.starts_with(' ')) {
        return None;
    }
    let body = body.trim();
    (!body.is_empty()).then(|| (label.to_string(), body.to_string()))
}

/// Strip a trailing `(recommended)` style marker; reports whether one was there.
fn strip_recommended(option_text: &str) -> (String, bool) {
    let lower = option_text.to_ascii_lowercase();
    for marker in [
        "(recommended)",
        "[recommended]",
        "(recommend)",
        "— recommended",
        "- recommended",
        "(★)",
        "★",
    ] {
        if let Some(at) = lower.find(marker) {
            let mut cleaned = String::new();
            cleaned.push_str(option_text[..at].trim_end());
            cleaned.push_str(option_text[at + marker.len()..].trim_end());
            return (cleaned.trim().to_string(), true);
        }
    }
    (option_text.to_string(), false)
}

/// The option label named by a `Recommend: A` / `I recommend option B` line.
fn recommended_label(line: &str) -> Option<(String, String)> {
    let lower = line.to_ascii_lowercase();
    let at = lower.find("recommend")?;
    let mut rest = &line[at + "recommend".len()..];
    rest = rest.trim_start_matches(|c: char| c.is_ascii_alphabetic() && c != ' ');
    let mut rest = rest.trim_start_matches([':', '-', '—', '–', ' ', '(', '*']);
    for word in [
        "option ", "answer ", "choice ", "Option ", "Answer ", "Choice ",
    ] {
        if let Some(stripped) = rest.strip_prefix(word) {
            rest = stripped;
            break;
        }
    }
    let n = label_len(rest);
    if n == 0 {
        return None;
    }
    let (label, tail) = rest.split_at(n);
    let upper_or_digit = label
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase());
    let ends = tail.chars().next().is_none_or(|c| !c.is_alphanumeric());
    let bare = tail
        .trim_start_matches([')', '.', ',', ';', '*'])
        .trim()
        .is_empty();
    if !ends || !(upper_or_digit || bare) {
        return None;
    }
    let remainder = tail
        .trim_start_matches([')', '.', ',', ';', ':', '-', '—', '*', ' '])
        .trim();
    Some((label.to_ascii_uppercase(), remainder.to_string()))
}

#[must_use]
pub fn parse_question(question: &str) -> ParsedQuestion {
    let mut lines = question.lines().map(str::trim_end).peekable();
    while lines.next_if(|l| l.trim().is_empty()).is_some() {}
    let mut parsed = ParsedQuestion::default();
    let mut first = true;
    let mut recommended_marker = None;
    let mut recommended_line = None;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((label, body)) = option_marker(line) {
            if !(first && parsed.options.is_empty() && parsed.headline.is_empty()) {
                let (body, marked) = strip_recommended(&body);
                if marked {
                    recommended_marker = Some(parsed.options.len());
                }
                parsed.options.push(DecisionOption { label, text: body });
                first = false;
                continue;
            }
        }
        // An indented line straight after an option continues that option.
        if let Some(last) = parsed.options.last_mut() {
            let indent = line.len() - line.trim_start().len();
            if indent >= 2 && recommended_label(line).is_none() {
                last.text.push(' ');
                last.text.push_str(line.trim());
                continue;
            }
        }
        if let Some((label, remainder)) = recommended_label(line) {
            recommended_line = Some(label);
            if !remainder.is_empty() {
                parsed.context.push(line.trim().to_string());
            }
            first = false;
            continue;
        }
        if parsed.headline.is_empty() && parsed.options.is_empty() {
            parsed.headline = line.trim().to_string();
        } else {
            parsed.context.push(line.trim().to_string());
        }
        first = false;
    }
    let distinct = parsed
        .options
        .iter()
        .enumerate()
        .all(|(i, o)| parsed.options[..i].iter().all(|p| p.label != o.label));
    if parsed.options.len() < 2 || !distinct {
        // Not a menu: keep the whole text readable as context instead.
        let options = std::mem::take(&mut parsed.options);
        for option in options {
            parsed
                .context
                .push(format!("{}) {}", option.label, option.text));
        }
        return parsed;
    }
    parsed.recommended = recommended_marker.or_else(|| {
        let label = recommended_line?;
        parsed
            .options
            .iter()
            .position(|o| o.label.eq_ignore_ascii_case(&label))
    });
    parsed
}

/// A field of the row, or of its `payload` when the row nests one.
fn field<'a>(row: &'a Value, name: &str) -> Option<&'a Value> {
    row.get(name)
        .filter(|v| !v.is_null())
        .or_else(|| row.get("payload").and_then(|p| p.get(name)))
        .filter(|v| !v.is_null())
}

/// The daemon's structured options: `[{label, detail?, recommended}]`.
/// `None` when the row carries none (an older record).
fn structured_options(row: &Value) -> Option<(Vec<DecisionOption>, Option<usize>)> {
    let items = field(row, "options")?.as_array()?;
    let mut options = Vec::new();
    let mut recommended = None;
    for item in items {
        let label = item.get("label").and_then(Value::as_str)?.trim();
        if label.is_empty() {
            continue;
        }
        let detail = item
            .get("detail")
            .and_then(Value::as_str)
            .map_or("", str::trim);
        if item.get("recommended").and_then(Value::as_bool) == Some(true) && recommended.is_none() {
            recommended = Some(options.len());
        }
        options.push(DecisionOption {
            label: label.to_string(),
            text: detail.to_string(),
        });
    }
    (!options.is_empty()).then_some((options, recommended))
}

/// The record's question as the board reads it: the headline and context come
/// from the question text, the options and recommendation from the structured
/// `options` when the daemon sent them, else from the question's own lines.
#[must_use]
pub fn parse_row(row: &Value) -> ParsedQuestion {
    let mut parsed = parse_question(text(row, "question").unwrap_or_default());
    let Some((options, recommended)) = structured_options(row) else {
        return parsed;
    };
    // The question may repeat the options as lines; show them once.
    parsed.context.retain(|line| {
        option_marker(line).is_none_or(|(label, _)| !options.iter().any(|o| o.label == label))
    });
    parsed.options = options;
    parsed.recommended = recommended;
    parsed
}

/// The record's status string (`pending`, `answered`, ...).
#[must_use]
pub fn status(row: &Value) -> &str {
    text(row, "status")
        .or_else(|| text(row, "state"))
        .unwrap_or("unknown")
}

#[must_use]
pub fn status_label(row: &Value) -> &'static str {
    if text(row, "type") == Some("operator_gate") {
        return "approval gate";
    }
    match status(row) {
        "pending" => "needs answer",
        "answer_queued" => "answer sending",
        "answered" => "answered",
        "declined" => "declined",
        "withdrawn" => "withdrawn",
        "archived" => "archived",
        "target_unavailable" => "target gone",
        "scope_revoked" => "scope revoked",
        "resolved_externally" => "resolved elsewhere",
        _ => "unknown state",
    }
}

fn parse_time(row: &Value, key: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text(row, key)?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Seconds since the record was asked.
#[must_use]
pub fn age_secs(row: &Value, now: DateTime<Utc>) -> Option<i64> {
    parse_time(row, "created_at").map(|t| (now - t).num_seconds().max(0))
}

/// Seconds since the record last changed.
#[must_use]
pub fn changed_secs(row: &Value, now: DateTime<Utc>) -> Option<i64> {
    parse_time(row, "updated_at").map(|t| (now - t).num_seconds().max(0))
}

/// Compact age, e.g. `42s`, `17m`, `3h`, `2d`; `?` when unknown.
#[must_use]
pub fn compact_age(seconds: Option<i64>) -> String {
    match seconds {
        None => "?".into(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86_400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86_400),
    }
}

#[must_use]
pub fn is_pending(row: &Value) -> bool {
    status(row) == "pending"
        || text(row, "gate_state") == Some("pending")
        || (text(row, "type") == Some("intent") && text(row, "state") == Some("blocked"))
}

/// A record the operator can still act on: pending, not stale-flagged away.
#[must_use]
pub fn stage(row: &Value, now: DateTime<Utc>) -> Stage {
    let flagged_stale = row
        .get("stale")
        .or_else(|| row.get("payload").and_then(|p| p.get("stale")))
        .and_then(Value::as_bool)
        == Some(true);
    match text(row, "type") {
        Some("decision" | "operator_gate" | "intent") | None if is_pending(row) => {
            if flagged_stale || age_secs(row, now).is_some_and(|a| a > STALE_AFTER_SECS) {
                Stage::Stale
            } else {
                Stage::NeedsYou
            }
        }
        Some("decision") if status(row) == "answer_queued" => Stage::InFlight,
        _ => Stage::Closed,
    }
}

/// Live records first: needs-you (longest waiting first), sending, stale
/// (newest first), then closed (most recently settled first).
pub fn sort_rows(rows: &mut [Value], now: DateTime<Utc>) {
    let key = |row: &Value| {
        let stage = stage(row, now);
        let age = age_secs(row, now).unwrap_or(0);
        let changed = changed_secs(row, now).unwrap_or(0);
        let order = match stage {
            Stage::NeedsYou => -age,
            Stage::InFlight | Stage::Closed => changed,
            Stage::Stale => age,
        };
        (
            stage,
            order,
            text(row, "key").unwrap_or_default().to_string(),
        )
    };
    rows.sort_by_cached_key(key);
}

/// The record's headline: the question's first line, else its key.
#[must_use]
pub fn headline(row: &Value) -> String {
    let parsed = parse_question(text(row, "question").unwrap_or_default());
    if parsed.headline.is_empty() {
        text(row, "key").unwrap_or("decision").to_string()
    } else {
        parsed.headline
    }
}

/// Names carried by a `blocks` style field: strings, or objects with a name.
#[must_use]
pub fn named_list(row: &Value, field: &str) -> Vec<String> {
    let value = row
        .get(field)
        .or_else(|| row.get("payload").and_then(|p| p.get(field)));
    let name = |v: &Value| -> Option<String> {
        v.as_str().map(str::to_string).or_else(|| {
            ["title", "name", "key", "epic_id", "id"]
                .iter()
                .find_map(|k| v.get(*k).and_then(Value::as_str))
                .map(str::to_string)
        })
    };
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(name).collect(),
        Some(Value::Null) | None => vec![],
        Some(other) => name(other).into_iter().collect(),
    }
}

/// Who may answer the record: `operator` (a gate) or `manager`.
#[must_use]
pub fn answerable_by(row: &Value) -> Option<&str> {
    field(row, "answerable_by")?.as_str()
}

/// Who ruled on the record when it was not the operator: the manager's node
/// label, else its kind (`project manager`). `None` for an operator answer or
/// an unanswered record.
#[must_use]
pub fn ruled_by(row: &Value) -> Option<String> {
    let by = field(row, "answered_by")?;
    if let Some(name) = by.as_str() {
        return (name != "operator").then(|| name.to_string());
    }
    let kind = by.get("kind").and_then(Value::as_str).unwrap_or_default();
    if kind == "operator" {
        return None;
    }
    let label = by
        .get("node_label")
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty());
    Some(label.map_or_else(|| kind.replace('_', " "), str::to_string))
}

/// The status as one phrase; a delegated ruling names the manager.
#[must_use]
pub fn status_text(row: &Value) -> String {
    match (status(row), ruled_by(row)) {
        ("answered", Some(manager)) => format!("ruled by {manager}"),
        _ => status_label(row).to_string(),
    }
}

/// Who may answer a pending record, in plain words.
#[must_use]
pub fn answerable_by_label(row: &Value) -> Option<String> {
    let who = field(row, "answerable_by")?.as_str()?;
    let gate = field(row, "gate")
        .and_then(Value::as_str)
        .map(|g| g.replace('_', " "));
    Some(match (who, gate) {
        ("operator", Some(gate)) => format!("you (operator) · a real gate: {gate}"),
        ("operator", None) => "you (operator)".to_string(),
        ("manager", _) => {
            "a delegated manager (not a gate); you can still answer it yourself".to_string()
        }
        (other, _) => other.to_string(),
    })
}

/// What an open record blocks: launches under named Epics, when the daemon
/// says it does. `None` once the block cleared.
#[must_use]
pub fn blocks_label(row: &Value) -> Option<String> {
    let blocks = field(row, "blocks")?;
    if blocks.is_object() {
        if blocks.get("launches").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let epics: Vec<String> = blocks
            .get("epic_ids")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(|id| id.chars().take(8).collect())
                    .collect()
            })
            .unwrap_or_default();
        return Some(if epics.is_empty() {
            "launches (create_session)".to_string()
        } else {
            format!("create_session under Epic {}", epics.join(", "))
        });
    }
    let names = named_list(row, "blocks");
    (!names.is_empty()).then(|| names.join(", "))
}

fn actor_name(actor: &Value) -> Option<String> {
    if let Some(name) = actor.as_str() {
        return (!name.is_empty()).then(|| name.to_string());
    }
    let kind = actor.get("kind").and_then(Value::as_str)?;
    let label = actor
        .get("node_label")
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty());
    Some(label.map_or_else(|| kind.replace('_', " "), str::to_string))
}

/// The daemon's audit trail (`history`), one `(age, what)` per entry.
fn recorded_history(row: &Value, now: DateTime<Utc>) -> Option<Vec<(String, String)>> {
    let entries = field(row, "history")?.as_array()?;
    let lines: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let event = entry.get("event").and_then(Value::as_str)?;
            let age = entry
                .get("at")
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| (now - t.with_timezone(&Utc)).num_seconds().max(0));
            let mut what = match event {
                "reasked" => "asked again".to_string(),
                other => other.replace('_', " "),
            };
            if let Some(actor) = entry.get("actor").and_then(actor_name) {
                what.push_str(&format!(" by {actor}"));
            }
            if let Some(note) = entry
                .get("note")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
            {
                what.push_str(&format!(": {note}"));
            }
            Some((compact_age(age), what))
        })
        .collect();
    (!lines.is_empty()).then_some(lines)
}

/// One timeline line per recorded fact: `(age, what happened)`.
#[must_use]
pub fn history(row: &Value, now: DateTime<Utc>) -> Vec<(String, String)> {
    if let Some(recorded) = recorded_history(row, now) {
        return recorded;
    }
    let mut lines = vec![(compact_age(age_secs(row, now)), "asked".to_string())];
    let version = number(row, "row_version").unwrap_or(1);
    let st = status(row);
    if st != "pending" || version > 1 {
        let mut what = status_label(row).to_string();
        if let Some(answer) = text(row, "answer").filter(|a| !a.is_empty()) {
            what.push_str(&format!(": {answer}"));
        }
        lines.push((compact_age(changed_secs(row, now)), what));
    }
    if let Some(state) = row
        .get("delivery")
        .or_else(|| row.get("payload").and_then(|p| p.get("delivery")))
        .and_then(|d| d.get("state"))
        .and_then(Value::as_str)
    {
        lines.push((
            compact_age(changed_secs(row, now)),
            format!("delivery {}", state.replace('_', " ")),
        ));
    }
    let retrieval = row
        .get("answer_retrieval")
        .or_else(|| row.get("payload").and_then(|p| p.get("answer_retrieval")));
    for role in ["manager", "lead"] {
        if let Some(at) = retrieval
            .and_then(|r| r.get(role))
            .and_then(|r| r.get("retrieved_at"))
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        {
            lines.push((
                compact_age(Some((now - at.with_timezone(&Utc)).num_seconds().max(0))),
                format!("read by the {role}"),
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn letter_options_with_a_recommend_line() {
        let p = parse_question(
            "Census access: how should E3 read it?\nA) Read the DB directly\nB) Call the API\nC) Read a nightly mirror\nRecommend: A",
        );
        assert_eq!(p.headline, "Census access: how should E3 read it?");
        assert_eq!(p.options.len(), 3);
        assert_eq!(p.options[2].label, "C");
        assert_eq!(p.options[2].text, "Read a nightly mirror");
        assert_eq!(p.recommended, Some(0));
        assert!(p.context.is_empty());
    }

    #[test]
    fn numbered_options_with_a_recommended_marker_strip_the_marker() {
        let p = parse_question("Who owns it?\n1. Platform\n2. Data team (recommended)\n3. Nobody");
        assert_eq!(p.recommended, Some(1));
        assert_eq!(p.options[1].text, "Data team");
        assert_eq!(p.options[1].answer(), "2: Data team");
    }

    #[test]
    fn recommendation_with_reason_keeps_the_reason_as_context() {
        let p = parse_question("Pick\nA) one\nB) two\nI recommend B because it is cheaper");
        assert_eq!(p.recommended, Some(1));
        assert_eq!(p.context, vec!["I recommend B because it is cheaper"]);
    }

    #[test]
    fn prose_recommendation_does_not_name_an_option_by_accident() {
        let p = parse_question("Pick\nA) one\nB) two\nWe recommend a quick spike first");
        assert_eq!(p.recommended, None);
    }

    #[test]
    fn a_single_marker_or_none_is_free_text() {
        let p = parse_question("Name the contact.\nA) only one");
        assert!(p.options.is_empty());
        assert_eq!(p.headline, "Name the contact.");
        assert_eq!(p.context, vec!["A) only one"]);
        assert!(parse_question("Plain question?").options.is_empty());
    }

    #[test]
    fn indented_lines_continue_the_option_above() {
        let p = parse_question("Pick\nA) first part\n   second part\nB) two");
        assert_eq!(p.options[0].text, "first part second part");
    }

    #[test]
    fn sorts_live_first_then_stale_then_closed() {
        let now = Utc::now();
        let at = |secs: i64| (now - chrono::Duration::seconds(secs)).to_rfc3339();
        let row = |key: &str, status: &str, age: i64| json!({"type":"decision","key":key,"status":status,"created_at":at(age),"updated_at":at(age)});
        let mut rows = vec![
            row("closed", "answered", 100),
            row("stale", "pending", 30 * 86_400),
            row("new", "pending", 60),
            row("old", "pending", 3600),
            row("sending", "answer_queued", 10),
        ];
        sort_rows(&mut rows, now);
        let keys: Vec<_> = rows.iter().map(|r| r["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["old", "new", "sending", "stale", "closed"]);
    }

    #[test]
    fn named_list_reads_strings_and_objects() {
        let row = json!({"blocks":["create_session in Census Epic",{"title":"Ingest Epic"}]});
        assert_eq!(
            named_list(&row, "blocks"),
            ["create_session in Census Epic", "Ingest Epic"]
        );
        assert!(named_list(&row, "missing").is_empty());
    }

    #[test]
    fn structured_options_win_over_the_question_text() {
        let row = json!({"type":"decision","key":"k","status":"pending",
            "question":"Pick a mirror\nA) Direct\nB) API",
            "options":[
                {"label":"A","detail":"Read the DB","recommended":false},
                {"label":"B","detail":"Call the API","recommended":true}]});
        let p = parse_row(&row);
        assert_eq!(p.headline, "Pick a mirror");
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.options[1].answer(), "B: Call the API");
        assert_eq!(p.recommended, Some(1));
        assert!(p.context.is_empty(), "{:?}", p.context);
    }

    #[test]
    fn a_label_only_option_answers_with_the_label() {
        let row = json!({"question":"Ship it?","options":[
            {"label":"Yes","recommended":true},{"label":"No"}]});
        let p = parse_row(&row);
        assert_eq!(p.options[0].answer(), "Yes");
        assert_eq!(p.recommended, Some(0));
    }

    #[test]
    fn rows_without_structured_options_fall_back_to_the_parser() {
        let row = json!({"question":"Pick\nA) one\nB) two\nRecommend: B"});
        let p = parse_row(&row);
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.recommended, Some(1));
    }

    #[test]
    fn a_ruling_names_the_manager_and_an_operator_answer_does_not() {
        let ruled = json!({"status":"answered","answered_by":
            {"kind":"portfolio_manager","session_id":"s","node_label":"Global","at":"x"}});
        assert_eq!(ruled_by(&ruled).as_deref(), Some("Global"));
        assert_eq!(status_text(&ruled), "ruled by Global");
        let unlabelled = json!({"status":"answered","answered_by":{"kind":"project_manager"}});
        assert_eq!(ruled_by(&unlabelled).as_deref(), Some("project manager"));
        let operator = json!({"status":"answered","answered_by":{"kind":"operator"}});
        assert_eq!(ruled_by(&operator), None);
        assert_eq!(status_text(&operator), "answered");
    }

    #[test]
    fn withdrawn_is_its_own_status_and_stage() {
        let row = json!({"type":"decision","status":"withdrawn"});
        assert_eq!(status_text(&row), "withdrawn");
        assert_eq!(stage(&row, Utc::now()), Stage::Closed);
    }

    #[test]
    fn who_may_answer_and_what_it_blocks_read_in_plain_words() {
        let gate = json!({"answerable_by":"operator","gate":"spend"});
        assert_eq!(
            answerable_by_label(&gate).as_deref(),
            Some("you (operator) · a real gate: spend")
        );
        let delegated = json!({"answerable_by":"manager"});
        assert!(
            answerable_by_label(&delegated)
                .unwrap()
                .contains("delegated manager")
        );
        let blocking =
            json!({"blocks":{"launches":true,"epic_ids":["12345678-aaaa"],"reason":"r"}});
        assert_eq!(
            blocks_label(&blocking).as_deref(),
            Some("create_session under Epic 12345678")
        );
        let cleared = json!({"blocks":{"launches":false,"epic_ids":["12345678-aaaa"]}});
        assert_eq!(blocks_label(&cleared), None);
    }

    #[test]
    fn recorded_history_replaces_the_synthetic_timeline() {
        let now = Utc::now();
        let at = |s: i64| (now - chrono::Duration::seconds(s)).to_rfc3339();
        let row = json!({"created_at":at(7200),"history":[
            {"event":"asked","at":at(7200),"actor":{"kind":"project_manager","node_label":"Koplik"}},
            {"event":"withdrawn","at":at(60),"actor":"Koplik","note":"superseded"}]});
        let lines = history(&row, now);
        assert_eq!(lines[0], ("2h".into(), "asked by Koplik".into()));
        assert_eq!(
            lines[1],
            ("1m".into(), "withdrawn by Koplik: superseded".into())
        );
    }
}
