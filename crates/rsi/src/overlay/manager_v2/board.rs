use super::{ManagerSection, ManagerSurface};
use crate::{app::App, types::OverlayState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::{harness_manager::HarnessManagerConfigV1, harness_manager_v2::*};
use serde_json::Value;
use uuid::Uuid;

pub const SECTIONS: [ManagerInspectSectionV2; 11] = [
    ManagerInspectSectionV2::Overview,
    ManagerInspectSectionV2::Workers,
    ManagerInspectSectionV2::Work,
    ManagerInspectSectionV2::Requests,
    ManagerInspectSectionV2::Decisions,
    ManagerInspectSectionV2::Topology,
    ManagerInspectSectionV2::Resources,
    ManagerInspectSectionV2::Actions,
    ManagerInspectSectionV2::Events,
    ManagerInspectSectionV2::Archive,
    ManagerInspectSectionV2::Health,
];

pub struct BoardState {
    pub project_id: Uuid,
    pub identity: String,
    pub query: AgentManagerInspectRequestV2,
    pub inspection: ManagerInspectionV2,
    pub previous: Vec<Option<String>>,
    pub selected: usize,
    pub detail_scroll: u16,
    pub error: Option<String>,
    pub notice: String,
    pub answer: Option<DecisionDraft>,
    /// First pages of Decisions, Requests and Health while the composite
    /// Board is shown; `inspection` then holds the Overview first page.
    pub composite: Option<BoardPages>,
}
pub struct BoardPages {
    pub decisions: ManagerInspectionV2,
    pub requests: ManagerInspectionV2,
    /// First Health page: one daemon-built row per scoped Epic (#627).
    pub health: ManagerInspectionV2,
    /// Overview KPI rows (program/product). The daemon sorts Overview by key,
    /// so earlier-sorting signal rows can push them past the first page; they
    /// are gathered from at most `KPI_OVERVIEW_PAGES` Overview pages.
    pub kpis: Vec<Value>,
    /// A KPI row is still missing and Overview pages remain past the bound.
    pub kpis_beyond: bool,
}
/// Bound on Overview pages read to find the program/product KPI rows.
pub const KPI_OVERVIEW_PAGES: usize = 4;
/// A selectable Board entry: a band row, or a band's overflow line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardEntry {
    /// Band and index into the band's source page.
    Row(Band, usize),
    /// The band's `+more` line; Enter opens the owning full section.
    More(Band),
}
/// Rows shown per band before the overflow hint; the full section pages.
pub const BAND_ROWS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Decisions,
    Requests,
    Leads,
    Health,
    Signals,
}
impl Band {
    pub const ALL: [Self; 5] = [
        Self::Decisions,
        Self::Requests,
        Self::Leads,
        Self::Health,
        Self::Signals,
    ];
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Decisions => "DECISIONS",
            Self::Requests => "REQUESTS",
            Self::Leads => "LEADS",
            Self::Health => "HEALTH",
            Self::Signals => "SIGNALS",
        }
    }
    /// The inspect section whose first page supplies this band's rows.
    #[must_use]
    pub const fn source(self) -> ManagerInspectSectionV2 {
        match self {
            Self::Decisions => ManagerInspectSectionV2::Decisions,
            Self::Requests => ManagerInspectSectionV2::Requests,
            Self::Health => ManagerInspectSectionV2::Health,
            Self::Leads | Self::Signals => ManagerInspectSectionV2::Overview,
        }
    }
    /// Overflow hint naming the full section and its direct key.
    #[must_use]
    pub const fn more_hint(self) -> &'static str {
        match self {
            Self::Decisions => "+more · Enter here opens full Decisions (2)",
            Self::Requests => "+more · Enter here opens full Inbox (3)",
            Self::Health => "+more · Enter here opens full Inspect Health",
            Self::Leads | Self::Signals => "+more · Enter here opens full Inspect Overview",
        }
    }
    /// The full surface section that owns this band's rows.
    #[must_use]
    pub const fn owner(self) -> ManagerSection {
        match self {
            Self::Decisions => ManagerSection::Decisions,
            Self::Requests => ManagerSection::Inbox,
            Self::Health => ManagerSection::Inspect(ManagerInspectSectionV2::Health),
            Self::Leads | Self::Signals => {
                ManagerSection::Inspect(ManagerInspectSectionV2::Overview)
            }
        }
    }
    /// Overview KPI rows feed the KPI line; every other returned row lands in
    /// exactly one band so nothing is silently dropped.
    fn admits(self, row: &Value) -> bool {
        let kind = text(row, "type");
        match self {
            Self::Decisions | Self::Requests | Self::Health => true,
            Self::Leads => kind == Some("lead_control"),
            Self::Signals => !matches!(kind, Some("lead_control" | "overview")),
        }
    }
}
pub struct BandView {
    pub band: Band,
    /// Matching rows on the source first page.
    pub total: usize,
    /// Indices into the source page shown in the band.
    pub rows: Vec<usize>,
    /// The source page has more pages.
    pub more_pages: bool,
    /// First matching source-page index not shown in the band.
    pub first_hidden: Option<usize>,
}
impl BandView {
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.more_pages || self.total > self.rows.len()
    }
}
#[must_use]
pub fn band_row_label(band: Band, row: &Value) -> String {
    match band {
        Band::Requests => format!(
            "{} · {}",
            text(row, "message")
                .and_then(|m| m.lines().next())
                .or_else(|| text(row, "key"))
                .unwrap_or("Request"),
            text(row, "state").unwrap_or("unknown")
        ),
        Band::Leads => format!(
            "{} · fence {}",
            text(row, "title")
                .or_else(|| text(row, "epic_id"))
                .unwrap_or("Epic"),
            text(row, "fence_state").unwrap_or("unknown")
        ),
        Band::Health => health_label(row),
        Band::Decisions | Band::Signals => row_title(row),
    }
}
/// Compact age, e.g. `42s`, `17m`, `3h`, `2d`; `?` when the daemon had none.
fn compact_age(seconds: Option<i64>) -> String {
    match seconds {
        None => "?".into(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86_400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86_400),
    }
}
/// One HEALTH band line: Epic, every stuck code (`ok` only for an empty list
/// with a healthy current lead), lead status+age, live children and pending
/// notices. Details come from the generic renderer.
fn health_label(row: &Value) -> String {
    let lead = row.get("lead").filter(|lead| !lead.is_null());
    // `ok` needs both an empty stuck list and a visibly healthy current lead;
    // a Failed lead with a daemon-known retry owner still reads `unverified`.
    let lead_healthy = text(row, "lead_state") == Some("current")
        && lead.is_some_and(|lead| {
            !matches!(
                text(lead, "status"),
                None | Some("Failed" | "Interrupted" | "Archived" | "Deleted")
            )
        });
    let lead = lead.map_or_else(
        || format!("lead {}", text(row, "lead_state").unwrap_or("unknown")),
        |lead| {
            format!(
                "lead {} {}",
                text(lead, "status").unwrap_or("unknown"),
                compact_age(number(lead, "age_seconds"))
            )
        },
    );
    let pending: i64 = ["to_manager", "to_lead"]
        .iter()
        .filter_map(|d| row.get("notices")?.get(d)?.get("pending")?.as_i64())
        .sum();
    let stuck: Option<Vec<&str>> = row
        .get("stuck")
        .and_then(Value::as_array)
        .map(|codes| codes.iter().filter_map(|c| text(c, "code")).collect());
    let verdict = match stuck {
        Some(codes) if !codes.is_empty() => format!("stuck {}", codes.join(",")),
        Some(_) if lead_healthy => "ok".to_string(),
        Some(_) => "unverified".to_string(),
        None => "stuck unknown".to_string(),
    };
    // Stuck codes lead: a narrow band column clips the tail, not the signal.
    let mut line = format!(
        "{} · {} · {lead} · live {} · notices {pending}",
        text(row, "title")
            .or_else(|| text(row, "epic_id"))
            .unwrap_or("Epic"),
        verdict,
        row.get("children")
            .and_then(|c| c.get("live"))
            .and_then(Value::as_i64)
            .map_or_else(|| "?".into(), |n| n.to_string()),
    );
    if row.get("complete") == Some(&Value::Bool(false)) {
        line.push_str(" · bounded");
    }
    line
}
fn kpi_value(row: &Value, key: &str) -> String {
    match row_payload(row).get(key).or_else(|| row.get(key)) {
        Some(Value::Number(n)) => n.to_string(),
        _ => "unknown".into(),
    }
}
pub struct DecisionDraft {
    pub target: AnswerHarnessManagerDecisionRequestV2,
    pub question: String,
}

pub fn row_payload(row: &Value) -> &Value {
    row.get("payload").unwrap_or(row)
}
fn text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .or_else(|| row_payload(row).get(key).and_then(Value::as_str))
}
fn number(row: &Value, key: &str) -> Option<i64> {
    row.get(key)
        .and_then(Value::as_i64)
        .or_else(|| row_payload(row).get(key).and_then(Value::as_i64))
}

fn action_operation(row: &Value) -> Option<&Value> {
    row.get("operation")
}

fn action_kind(row: &Value) -> Option<&str> {
    text(row, "action_kind")
        .or_else(|| {
            row.get("outcome")
                .and_then(|receipt| receipt.get("action_kind"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            action_operation(row)
                .and_then(|operation| operation.get("action"))
                .and_then(Value::as_str)
        })
}

fn action_target_type(row: &Value) -> Option<&str> {
    text(row, "target_type")
        .or_else(|| {
            row.get("outcome")
                .and_then(|receipt| receipt.get("target_type"))
                .and_then(Value::as_str)
        })
        .or_else(|| match action_kind(row)? {
            "succeed_manager" => Some("manager_session"),
            "resume_lead" | "pause_lead" | "assign_lead" => Some("epic_lead"),
            "retry_lead" | "replace_lead" | "create_session" | "archive_session"
            | "restore_session" | "update_session" => Some("provider_session"),
            "create_container" => match action_operation(row)
                .and_then(|operation| operation.get("kind"))
                .and_then(Value::as_str)
            {
                Some("Epic") => Some("epic_container"),
                Some("Group") => Some("group_container"),
                _ => Some("container"),
            },
            "update_container" | "archive_container" | "delete_container" | "restore_container" => {
                Some("container")
            }
            _ => None,
        })
}

fn action_result(row: &Value) -> Option<&Value> {
    row.get("result")
        .filter(|result| !result.is_null())
        .or_else(|| {
            row.get("outcome")
                .and_then(|receipt| receipt.get("result"))
                .filter(|result| !result.is_null())
        })
}

fn action_receipt_state(row: &Value) -> Option<&str> {
    text(row, "receipt_state")
        .or_else(|| {
            row.get("outcome")
                .and_then(|receipt| receipt.get("state"))
                .and_then(Value::as_str)
        })
        .or_else(|| text(row, "state"))
}

fn action_lead_state(row: &Value) -> Option<&str> {
    action_result(row)
        .and_then(|result| result.get("lead_state"))
        .and_then(Value::as_str)
}

fn action_target_state(row: &Value) -> Option<&str> {
    action_result(row)
        .and_then(|result| result.get("target_state"))
        .and_then(Value::as_str)
}

fn sentence_label(value: &str) -> String {
    let mut value = human_key(value);
    if let Some(first) = value.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    value
}

fn action_target_label(value: &str) -> String {
    match value {
        "provider_session" => "Agent/provider session".into(),
        _ => sentence_label(value),
    }
}

fn action_title(row: &Value) -> Option<String> {
    let action = sentence_label(action_kind(row)?);
    let target = action_target_label(action_target_type(row)?);
    let receipt = action_receipt_state(row)?;
    let receipt = if receipt == "queued" {
        "queued (admission only)"
    } else if receipt == "running" {
        "running (not complete)"
    } else {
        receipt
    };
    let mut parts = vec![action, target, receipt.into()];
    if let Some(target_state) = action_target_state(row) {
        parts.push(sentence_label(target_state));
    }
    if action_lead_state(row) == Some("unassigned") {
        parts.push("lead unassigned".into());
    }
    Some(parts.join(" · "))
}

pub fn row_title(row: &Value) -> String {
    if text(row, "type") == Some("action") {
        if let Some(title) = action_title(row) {
            return title;
        }
    }
    let name = [
        "title",
        "name",
        "question",
        "key",
        "session_id",
        "epic_id",
        "type",
    ]
    .iter()
    .find_map(|k| text(row, k))
    .unwrap_or("Record");
    let state = ["status", "state", "stage", "type"]
        .iter()
        .find_map(|k| text(row, k))
        .unwrap_or("unknown");
    format!("{name} · {state}")
}
/// Only explicit session identities in a returned projection can open a pane.
pub fn row_session_id(row: &Value) -> Option<Uuid> {
    [
        "current_session_id",
        "session_id",
        "target_session_id",
        "effective_recipient_session_id",
        "recipient_session_id",
        "id",
    ]
    .iter()
    .find_map(|key| text(row, key).and_then(|id| Uuid::parse_str(id).ok()))
    .or_else(|| {
        row.get("expected")
            .and_then(|v| v.get("lead_session_id"))
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
    })
}

fn human_key(key: &str) -> String {
    key.replace('_', " ")
}
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "unknown".into(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}
/// Render returned fields with readable labels, explicit unknowns and bounds.
/// No completion or denominator is inferred from provider session status.
pub fn row_details(row: &Value) -> Vec<String> {
    fn walk(
        value: &Value,
        prefix: &str,
        depth: usize,
        action_semantics: bool,
        out: &mut Vec<String>,
        truncated: &mut bool,
    ) {
        if depth > 32 || out.len() >= 512 {
            *truncated = true;
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let semantic_duplicate = action_semantics
                        && ((prefix.is_empty()
                            && matches!(
                                key.as_str(),
                                "action_kind"
                                    | "target_type"
                                    | "receipt_state"
                                    | "result"
                                    | "state"
                            ))
                            || (prefix == "outcome"
                                && matches!(
                                    key.as_str(),
                                    "action_kind" | "target_type" | "result" | "state"
                                )));
                    if semantic_duplicate {
                        continue;
                    }
                    if out.len() >= 512 {
                        *truncated = true;
                        break;
                    }
                    let name = if prefix.is_empty() || key == "payload" {
                        human_key(key)
                    } else {
                        format!("{prefix} / {}", human_key(key))
                    };
                    if key == "payload" {
                        walk(value, "", depth + 1, action_semantics, out, truncated)
                    } else {
                        walk(value, &name, depth + 1, action_semantics, out, truncated)
                    }
                }
            }
            Value::Array(values) => {
                if values.is_empty() {
                    out.push(format!("{prefix}: none"));
                }
                for (i, value) in values.iter().take(512).enumerate() {
                    walk(
                        value,
                        &format!("{prefix} [{}]", i + 1),
                        depth + 1,
                        action_semantics,
                        out,
                        truncated,
                    );
                }
                *truncated |= values.len() > 512;
            }
            _ => out.push(format!("{prefix}: {}", scalar(value))),
        }
    }
    let action_semantics = text(row, "type") == Some("action") && action_kind(row).is_some();
    let mut lines = if action_semantics {
        let mut lines = vec![
            format!(
                "Action kind: {}",
                sentence_label(action_kind(row).unwrap_or("unknown"))
            ),
            format!(
                "Target type: {}",
                action_target_label(action_target_type(row).unwrap_or("unknown"))
            ),
        ];
        match action_receipt_state(row).unwrap_or("unknown") {
            "queued" => {
                lines.push("Receipt state: queued — admission only; no completion confirmed".into())
            }
            "running" => lines.push(
                "Receipt state: running — execution in progress; no completion confirmed".into(),
            ),
            state => lines.push(format!("Receipt state: {state}")),
        }
        if let Some(target_state) = action_target_state(row) {
            lines.push(format!("Target state: {}", sentence_label(target_state)));
        }
        if let Some(lead_state) = action_lead_state(row) {
            lines.push(match lead_state {
                "unassigned" => "Lead unassigned".into(),
                state => format!("Lead state: {}", sentence_label(state)),
            });
        }
        lines
    } else {
        vec![]
    };
    let mut truncated = false;
    walk(row, "", 0, action_semantics, &mut lines, &mut truncated);
    if truncated {
        lines.push("Details truncated: this record exceeds the display bound; remaining fields are unknown.".into());
    }
    lines
}

impl BoardState {
    #[must_use]
    pub fn new(
        project_id: Uuid,
        identity: String,
        query: AgentManagerInspectRequestV2,
        inspection: ManagerInspectionV2,
    ) -> Self {
        Self {
            project_id,
            identity,
            query,
            inspection,
            previous: vec![],
            selected: 0,
            detail_scroll: 0,
            error: None,
            answer: None,
            composite: None,
            notice: "Reported work, accepted work and integrated delivery are separate. Null evidence is unknown.".into(),
        }
    }
    /// Show the composite Board from its three first pages.
    pub fn install_board(&mut self, overview: ManagerInspectionV2, pages: BoardPages) {
        self.query = AgentManagerInspectRequestV2::default();
        self.previous.clear();
        self.inspection = overview;
        self.composite = Some(pages);
        self.selected = 0;
        self.detail_scroll = 0;
        self.error = None;
    }
    /// Show one full section page; leaves the composite Board.
    pub fn install_page(
        &mut self,
        query: AgentManagerInspectRequestV2,
        previous: Vec<Option<String>>,
        page: ManagerInspectionV2,
    ) {
        self.query = query;
        self.previous = previous;
        self.inspection = page;
        self.composite = None;
        self.selected = 0;
        self.detail_scroll = 0;
        self.error = None;
    }
    #[must_use]
    pub fn origin_page(&self, section: ManagerInspectSectionV2) -> Option<&ManagerInspectionV2> {
        match (&self.composite, section) {
            (Some(pages), ManagerInspectSectionV2::Decisions) => Some(&pages.decisions),
            (Some(pages), ManagerInspectSectionV2::Requests) => Some(&pages.requests),
            (Some(pages), ManagerInspectSectionV2::Health) => Some(&pages.health),
            (Some(_), ManagerInspectSectionV2::Overview) => Some(&self.inspection),
            (None, s) if s == self.query.section => Some(&self.inspection),
            _ => None,
        }
    }
    #[must_use]
    pub fn bands(&self) -> Vec<BandView> {
        if self.composite.is_none() {
            return vec![];
        }
        Band::ALL
            .iter()
            .filter_map(|band| {
                let page = self.origin_page(band.source())?;
                let matching: Vec<usize> = page
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| band.admits(row))
                    .map(|(i, _)| i)
                    .collect();
                Some(BandView {
                    band: *band,
                    total: matching.len(),
                    first_hidden: matching.get(BAND_ROWS).copied(),
                    rows: matching.into_iter().take(BAND_ROWS).collect(),
                    more_pages: page.next_cursor.is_some(),
                })
            })
            .collect()
    }
    /// Selectable Board entries in display order; j/k crosses band
    /// boundaries. A truncated band's `+more` line is selectable so an empty
    /// first page with a continuation cursor still has an Enter path.
    #[must_use]
    pub fn entries(&self) -> Vec<BoardEntry> {
        self.bands()
            .into_iter()
            .flat_map(|view| {
                let more = view.truncated().then_some(BoardEntry::More(view.band));
                view.rows
                    .into_iter()
                    .map(move |i| BoardEntry::Row(view.band, i))
                    .chain(more)
            })
            .collect()
    }
    #[must_use]
    pub fn row_count(&self) -> usize {
        if self.composite.is_some() {
            self.entries().len()
        } else {
            self.inspection.rows.len()
        }
    }
    /// The selected row's origin section and index in that section's page.
    #[must_use]
    pub fn selected_origin(&self) -> Option<(ManagerInspectSectionV2, usize)> {
        if self.composite.is_some() {
            match self.entries().get(self.selected)? {
                BoardEntry::Row(band, i) => Some((band.source(), *i)),
                BoardEntry::More(_) => None,
            }
        } else {
            Some((self.query.section, self.selected))
        }
    }
    #[must_use]
    pub fn selected_row(&self) -> Option<&Value> {
        let (section, index) = self.selected_origin()?;
        self.origin_page(section)?.rows.get(index)
    }
    /// Overview KPI rows (program/product), one line each.
    #[must_use]
    pub fn kpi_lines(&self) -> Vec<String> {
        let Some(pages) = &self.composite else {
            return vec![];
        };
        let mut lines: Vec<String> = pages
            .kpis
            .iter()
            .map(|row| {
                let mut line = format!(
                    "{} accepted {} · integrated {} / {} · ready {} · partial {} · unknown {}",
                    text(row, "kind").unwrap_or("work"),
                    kpi_value(row, "accepted"),
                    kpi_value(row, "integrated"),
                    kpi_value(row, "denominator"),
                    kpi_value(row, "ready"),
                    kpi_value(row, "partial"),
                    kpi_value(row, "unknown"),
                );
                match row_payload(row).get("missing_work_scope") {
                    Some(Value::Bool(true)) => line.push_str(" · missing work scope"),
                    Some(Value::Bool(false)) => {}
                    _ => line.push_str(" · work scope unknown"),
                }
                line
            })
            .collect();
        if pages.kpis_beyond {
            lines.push(format!(
                "KPIs beyond {KPI_OVERVIEW_PAGES} Overview pages not loaded · Enter on a SIGNALS row, then n pages Overview"
            ));
        } else if lines.is_empty() {
            lines.push("KPIs unknown: the daemon returned no overview KPI rows.".into());
        }
        lines
    }
    /// Appointed manager seat state from the Overview KPI rows (#669):
    /// `(label, down)`. `None` when the daemon reported neither the durable
    /// seat record nor `manager_available`.
    #[must_use]
    pub fn seat_label(&self) -> Option<(String, bool)> {
        let pages = self.composite.as_ref()?;
        let row = pages.kpis.iter().map(row_payload).find(|row| {
            row.get("manager_seat").is_some_and(|seat| !seat.is_null())
                || row.get("manager_available").is_some()
        })?;
        if let Some(seat) = row.get("manager_seat").filter(|seat| !seat.is_null()) {
            let state = seat
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let count = |key| seat.get(key).and_then(Value::as_u64).unwrap_or(0);
            let reason = seat
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Some(match state {
                "live" => ("seat live".into(), false),
                "recovering" => (
                    format!(
                        "seat RECOVERING {}/{} ({reason})",
                        count("attempts"),
                        count("max_attempts")
                    ),
                    true,
                ),
                "exhausted" => (
                    format!(
                        "seat EXHAUSTED {}/{} ({reason})",
                        count("attempts"),
                        count("max_attempts")
                    ),
                    true,
                ),
                "down" => (format!("seat DOWN ({reason})"), true),
                other => (format!("seat {other}"), true),
            });
        }
        Some(
            match row.get("manager_available").and_then(Value::as_bool) {
                Some(true) => ("seat live".into(), false),
                _ => ("seat unavailable".into(), true),
            },
        )
    }
    /// Traversal coverage across the Board's four first pages.
    #[must_use]
    pub fn coverage_label(&self) -> String {
        let Some(pages) = &self.composite else {
            return String::new();
        };
        let open: Vec<String> = [
            ("overview", &self.inspection),
            ("decisions", &pages.decisions),
            ("requests", &pages.requests),
            ("health", &pages.health),
        ]
        .iter()
        .filter_map(|(name, page)| {
            if page.next_cursor.is_some() {
                Some(format!("{name} +more"))
            } else if !page.complete {
                Some(format!("{name} partial"))
            } else {
                None
            }
        })
        .collect();
        if open.is_empty() {
            "complete".into()
        } else {
            open.join(", ")
        }
    }
    /// Leave the Board for the section that owns the selected row, reusing
    /// the already-loaded first page with that row selected.
    pub fn drill(&mut self) -> Option<ManagerSection> {
        let (band, index) = match self.entries().get(self.selected).copied()? {
            BoardEntry::Row(band, index) => (band, index),
            BoardEntry::More(band) => (
                band,
                self.bands()
                    .iter()
                    .find(|view| view.band == band)
                    .and_then(|view| view.first_hidden)
                    .unwrap_or(0),
            ),
        };
        let pages = self.composite.take()?;
        let page = match band.source() {
            ManagerInspectSectionV2::Decisions => pages.decisions,
            ManagerInspectSectionV2::Requests => pages.requests,
            ManagerInspectSectionV2::Health => pages.health,
            // Overview already lives in `inspection`; the placeholder is
            // overwritten by `install_page` below.
            _ => std::mem::replace(&mut self.inspection, pages.decisions),
        };
        self.install_page(
            AgentManagerInspectRequestV2 {
                section: band.source(),
                ..Default::default()
            },
            vec![],
            page,
        );
        self.selected = index;
        Some(band.owner())
    }
    pub fn decision_draft(&self) -> Result<DecisionDraft, String> {
        let (section, index) = self.selected_origin().ok_or("Select a pending decision.")?;
        if section != ManagerInspectSectionV2::Decisions && self.composite.is_some() {
            return Err("a answers decision rows; select a row in DECISIONS.".into());
        }
        let page = self
            .origin_page(section)
            .ok_or("Select a pending decision.")?;
        let row = page.rows.get(index).ok_or("Select a pending decision.")?;
        if text(row, "route_state") == Some("unavailable") {
            return Err(text(row, "next_action")
                .unwrap_or("Open this session to inspect its unresolved gate.")
                .into());
        }
        let status = text(row, "status")
            .or_else(|| text(row, "state"))
            .ok_or("Decision state is unknown. Refresh before answering.")?;
        if status != "pending" {
            return Err("This decision is not pending. Refresh to inspect its outcome.".into());
        }
        let key = text(row, "key")
            .or_else(|| text(row, "decision_key"))
            .ok_or("Decision identity unavailable.")?;
        let version = number(row, "row_version")
            .filter(|v| *v > 0)
            .ok_or("Decision version unavailable.")?;
        let digest = text(row, "target_digest")
            .filter(|s| !s.is_empty())
            .ok_or("Exact decision target digest unavailable.")?;
        let policy = page
            .policy
            .as_ref()
            .filter(|p| !p.revoked && p.scope_version == page.scope_version)
            .ok_or("A current operator policy is required. Use :manager policy.")?;
        Ok(DecisionDraft {
            question: text(row, "question").unwrap_or(key).into(),
            target: AnswerHarnessManagerDecisionRequestV2 {
                project_id: self.project_id,
                fence: ManagerFenceV2 {
                    scope_version: page.scope_version,
                    policy_version: policy.row_version,
                },
                decision_key: key.into(),
                expected_row_version: version,
                target_digest: digest.into(),
                answer: String::new(),
                idempotency_key: Uuid::new_v4().to_string(),
            },
        })
    }
}

#[allow(clippy::future_not_send)]
async fn fetch_page(
    app: &mut App,
    project_id: Uuid,
    section: ManagerInspectSectionV2,
    cursor: Option<String>,
) -> Result<ManagerInspectionV2, String> {
    app.client
        .get_harness_manager_state(GetHarnessManagerStateRequestV2 {
            project_id,
            query: AgentManagerInspectRequestV2 {
                section,
                cursor,
                ..Default::default()
            },
        })
        .await
        .map_err(|e| format!("{section:?}: {e}"))
}

fn kpi_rows(page: &ManagerInspectionV2) -> impl Iterator<Item = &Value> {
    page.rows
        .iter()
        .filter(|row| text(row, "type") == Some("overview"))
}
fn has_all_kpis(kpis: &[Value]) -> bool {
    ["program", "product"]
        .iter()
        .all(|kind| kpis.iter().any(|row| text(row, "kind") == Some(kind)))
}

/// The Board's bounded load: four first pages (Overview, Decisions,
/// Requests, Health), plus at most `KPI_OVERVIEW_PAGES - 1` Overview follow-up pages
/// only while a program/product KPI row is still missing.
#[allow(clippy::future_not_send)]
pub(super) async fn fetch_board(
    app: &mut App,
    project_id: Uuid,
) -> Result<(ManagerInspectionV2, BoardPages), String> {
    use ManagerInspectSectionV2::{Decisions, Health, Overview, Requests};
    let overview = fetch_page(app, project_id, Overview, None).await?;
    let mut kpis: Vec<Value> = kpi_rows(&overview).cloned().collect();
    let mut cursor = overview.next_cursor.clone();
    let mut read = 1;
    while !has_all_kpis(&kpis) && read < KPI_OVERVIEW_PAGES {
        let Some(next) = cursor.take() else { break };
        let page = fetch_page(app, project_id, Overview, Some(next)).await?;
        kpis.extend(kpi_rows(&page).cloned());
        cursor = page.next_cursor;
        read += 1;
    }
    let kpis_beyond = !has_all_kpis(&kpis) && cursor.is_some();
    let decisions = fetch_page(app, project_id, Decisions, None).await?;
    let requests = fetch_page(app, project_id, Requests, None).await?;
    let health = fetch_page(app, project_id, Health, None).await?;
    Ok((
        overview,
        BoardPages {
            decisions,
            requests,
            health,
            kpis,
            kpis_beyond,
        },
    ))
}

#[allow(clippy::future_not_send)]
pub(super) async fn open_section(
    app: &mut App,
    config: HarnessManagerConfigV1,
    identity: String,
    section: ManagerSection,
) -> Result<(), String> {
    let query = AgentManagerInspectRequestV2 {
        section: section.inspect_section(),
        ..Default::default()
    };
    let ledger = if section == ManagerSection::Board {
        let (overview, pages) = fetch_board(app, config.project_id)
            .await
            .map_err(|e| format!("Manager board: {e}"))?;
        let mut ledger = BoardState::new(config.project_id, identity.clone(), query, overview);
        ledger.composite = Some(pages);
        ledger
    } else {
        let inspection = app
            .client
            .get_harness_manager_state(GetHarnessManagerStateRequestV2 {
                project_id: config.project_id,
                query: query.clone(),
            })
            .await
            .map_err(|e| format!("Manager board: {e}"))?;
        BoardState::new(config.project_id, identity.clone(), query, inspection)
    };
    app.overlay_leader_pending = false;
    app.overlay = OverlayState::HarnessManagerV2(Box::new(ManagerSurface {
        project_id: config.project_id,
        identity: identity.clone(),
        section,
        ledger,
        policy: None,
        config: config.clone(),
    }));
    if section == ManagerSection::Policy {
        let mut state = super::policy::load(app, config, identity).await?;
        state.start_usage(app.client.socket_path().to_path_buf());
        if let Some(surface) = super::surface_mut(app) {
            surface.policy = Some(state);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) async fn open(
    app: &mut App,
    config: HarnessManagerConfigV1,
    identity: String,
    decisions: bool,
) -> Result<(), String> {
    open_section(
        app,
        config,
        identity,
        if decisions {
            ManagerSection::Decisions
        } else {
            ManagerSection::Board
        },
    )
    .await
}

async fn load(app: &mut App, query: AgentManagerInspectRequestV2, previous: Vec<Option<String>>) {
    let Some(state) = super::surface_mut(app).map(|s| &mut s.ledger) else {
        return;
    };
    let request = GetHarnessManagerStateRequestV2 {
        project_id: state.project_id,
        query: query.clone(),
    };
    let result = app.client.get_harness_manager_state(request).await;
    if let Some(state) = super::board_mut(app) {
        match result {
            Ok(page) => state.install_page(query, previous, page),
            Err(e) => {
                state.error = Some(format!(
                    "Board not refreshed: {e}. Existing page retained; r restarts paging."
                ))
            }
        }
    }
}

pub(super) async fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(state) = super::board_mut(app) else {
        return;
    };
    if let Some(draft) = &mut state.answer {
        match key.code {
            KeyCode::Esc => state.answer = None,
            KeyCode::Backspace => {
                draft.target.answer.pop();
                draft.target.idempotency_key = Uuid::new_v4().to_string();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                draft.target.answer.clear();
                draft.target.idempotency_key = Uuid::new_v4().to_string();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                draft.target.answer.push(c);
                draft.target.idempotency_key = Uuid::new_v4().to_string();
            }
            KeyCode::Enter => {
                if draft.target.answer.trim().is_empty() {
                    state.error = Some("Enter an answer; Esc keeps the decision pending.".into());
                    return;
                }
                let target = draft.target.clone();
                match app
                    .client
                    .answer_harness_manager_decision(target.clone())
                    .await
                {
                    Ok(receipt) => {
                        if let Some(state) = super::board_mut(app) {
                            state.answer = None;
                            state.error = None;
                            state.notice = format!(
                                "Answer receipt for {} (version {}): {}. r refreshes delivery and gate state.",
                                target.decision_key,
                                target.expected_row_version,
                                text(&receipt, "state")
                                    .or_else(|| text(&receipt, "status"))
                                    .unwrap_or("received; delivery outcome unknown")
                            );
                        }
                    }
                    Err(e) => {
                        if let Some(state) = super::board_mut(app) {
                            state.error = Some(format!(
                                "Answer not confirmed: {e}. Exact target and draft retained; Esc then r to refresh."
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
        return;
    }
    let rows = state.row_count();
    if super::super::list::handle_list_nav_key(&mut state.selected, rows, &key) {
        state.detail_scroll = 0;
        return;
    }
    let board = state.composite.is_some();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.overlay = OverlayState::None,
        KeyCode::Char('o') => {
            let target = state.selected_row().and_then(row_session_id);
            let Some(target) = target else {
                state.error = Some("This row has no session target.".into());
                return;
            };
            match app.client.get_session(target).await {
                Ok(session) => {
                    app.upsert_session(session);
                    app.overlay = OverlayState::None;
                    app.detail_list_focused = false;
                    app.open_session_in_current_pane(target);
                }
                Err(error) => {
                    if let Some(state) = super::board_mut(app) {
                        state.error = Some(format!("Session unavailable: {error}"));
                    }
                }
            }
        }
        KeyCode::Char('n' | 'p') if board => {
            state.notice = "Board shows first pages · Enter opens the full section to page.".into();
        }
        KeyCode::Char('r') if board => {
            let project_id = state.project_id;
            let result = fetch_board(app, project_id).await;
            if let Some(state) = super::board_mut(app) {
                match result {
                    Ok((overview, pages)) => state.install_board(overview, pages),
                    Err(e) => {
                        state.error = Some(format!(
                            "Board not refreshed: {e}. Existing pages retained."
                        ));
                    }
                }
            }
        }
        KeyCode::Enter if board => {
            if let Some(surface) = super::surface_mut(app) {
                match surface.ledger.drill() {
                    Some(section) => surface.section = section,
                    None => surface.ledger.error = Some("Select a row to open its section.".into()),
                }
            }
        }
        KeyCode::Char('a') if board => match state.decision_draft() {
            Ok(draft) => state.answer = Some(draft),
            Err(e) => state.error = Some(e),
        },
        KeyCode::Char('n') => {
            if let Some(next) = state.inspection.next_cursor.clone() {
                let mut previous = state.previous.clone();
                previous.push(state.query.cursor.clone());
                let mut query = state.query.clone();
                query.cursor = Some(next);
                load(app, query, previous).await;
            }
        }
        KeyCode::Char('p') => {
            let mut previous = state.previous.clone();
            if let Some(cursor) = previous.pop() {
                let mut query = state.query.clone();
                query.cursor = cursor;
                load(app, query, previous).await;
            }
        }
        KeyCode::Char('r') => {
            let mut query = state.query.clone();
            query.cursor = None;
            load(app, query, vec![]).await;
        }
        KeyCode::PageDown => state.detail_scroll = state.detail_scroll.saturating_add(8),
        KeyCode::PageUp => state.detail_scroll = state.detail_scroll.saturating_sub(8),
        KeyCode::Enter | KeyCode::Char('a')
            if state.query.section == ManagerInspectSectionV2::Decisions =>
        {
            match state.decision_draft() {
                Ok(draft) => state.answer = Some(draft),
                Err(e) => state.error = Some(e),
            }
        }
        _ => {}
    }
}
