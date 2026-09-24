//! `rsi-diag mismatch-report` — aggregate per-command capability-class
//! mismatches from `~/.rsi/rsi.db` (read-only).

use rsi_common::types::CapabilityClass;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

/// Stable ordering for grouped-output sort: by (declared, actual).
/// `CapabilityClass` doesn't implement `Ord`, so we project onto an `i32`.
fn class_ord(c: CapabilityClass) -> i32 {
    match c {
        CapabilityClass::Architect => 0,
        CapabilityClass::Implementer => 1,
        CapabilityClass::LookupFast => 2,
    }
}

/// One grouped row in the report: (declared, actual_classified, count, sample_model).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    declared: CapabilityClass,
    actual: Option<CapabilityClass>,
    count: u64,
    sample_model: String,
}

/// Parse a stored capability_class TEXT value (snake_case) into the enum.
fn parse_declared(s: &str) -> Option<CapabilityClass> {
    match s {
        "architect" => Some(CapabilityClass::Architect),
        "implementer" => Some(CapabilityClass::Implementer),
        "lookup_fast" => Some(CapabilityClass::LookupFast),
        _ => None,
    }
}

/// Render an `Option<CapabilityClass>` to the wire-format string used in the
/// output ("architect" / "implementer" / "lookup_fast" / "unknown").
fn class_label(c: Option<CapabilityClass>) -> &'static str {
    match c {
        Some(CapabilityClass::Architect) => "architect",
        Some(CapabilityClass::Implementer) => "implementer",
        Some(CapabilityClass::LookupFast) => "lookup_fast",
        None => "unknown",
    }
}

/// Aggregate raw (declared, model) rows into grouped counts by
/// (declared, classified_actual). Preserves sample_model for each group.
fn aggregate(rows: Vec<(String, String)>) -> Vec<Group> {
    let mut map: HashMap<(CapabilityClass, Option<CapabilityClass>), (u64, String)> =
        HashMap::new();
    for (declared_str, model) in rows {
        let Some(declared) = parse_declared(&declared_str) else {
            continue;
        };
        let actual = CapabilityClass::classify(&model);
        let entry = map.entry((declared, actual)).or_insert((0, model.clone()));
        entry.0 += 1;
    }

    let mut groups: Vec<Group> = map
        .into_iter()
        .map(|((declared, actual), (count, sample_model))| Group {
            declared,
            actual,
            count,
            sample_model,
        })
        .collect();
    groups.sort_by_key(|g| {
        (
            class_ord(g.declared),
            g.actual.map(class_ord).unwrap_or(i32::MAX),
        )
    });
    groups
}

/// Pretty-print the report as a human-readable table.
fn render_table(groups: &[Group]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<15} {:<15} {:<9} sample model",
        "declared", "actual", "sessions"
    );
    let mut total: u64 = 0;
    let mut mismatches: u64 = 0;
    for g in groups {
        total += g.count;
        let declared = class_label(Some(g.declared));
        let actual = class_label(g.actual);
        let marker = if g.actual.is_some_and(|a| a != g.declared) {
            mismatches += g.count;
            "  ← mismatch"
        } else if g.actual.is_none() {
            "  ← unknown model"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{:<15} {:<15} {:<9} {}{}",
            declared, actual, g.count, g.sample_model, marker
        );
    }
    out.push_str("──────────────────────────────────────────────────────────\n");
    let rate = if total == 0 {
        0.0
    } else {
        (mismatches as f64 * 100.0) / (total as f64)
    };
    let _ = writeln!(
        out,
        "Mismatch rate: {} / {} sessions ({:.1}%)",
        mismatches, total, rate
    );
    out
}

/// Render the report as a compact JSON array. Uses hand-rolled serialization
/// so we don't add `serde_json` to `rsi-diag`'s dep list.
fn render_json(groups: &[Group]) -> String {
    let mut out = String::from("[");
    for (i, g) in groups.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let declared = class_label(Some(g.declared));
        let actual = class_label(g.actual);
        let _ = write!(
            out,
            "{{\"declared\":\"{}\",\"actual\":\"{}\",\"count\":{},\"sample_model\":\"{}\"}}",
            declared,
            actual,
            g.count,
            g.sample_model.replace('"', "\\\"")
        );
    }
    out.push(']');
    out
}

/// Read session rows and produce the report. Opens the DB in `READ_ONLY` mode.
pub fn run(db_path: &Path, since: Option<&str>, json: bool) -> Result<(), String> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open {}: {}", db_path.display(), e))?;

    let (sql, params_vec): (&str, Vec<String>) = match since {
        Some(s) => (
            "SELECT capability_class, model FROM sessions \
             WHERE capability_class IS NOT NULL AND model IS NOT NULL \
             AND created_at >= ?1",
            vec![s.to_string()],
        ),
        None => (
            "SELECT capability_class, model FROM sessions \
             WHERE capability_class IS NOT NULL AND model IS NOT NULL",
            vec![],
        ),
    };

    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {}", e))?;
    let rows: Vec<(String, String)> = if params_vec.is_empty() {
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("row: {}", e))?
    } else {
        stmt.query_map(rusqlite::params![params_vec[0]], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("row: {}", e))?
    };

    let groups = aggregate(rows);
    let output = if json {
        render_json(&groups)
    } else {
        render_table(&groups)
    };
    print!("{}", output);
    if json {
        println!();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_groups_pairs_and_counts_correctly() {
        let rows = vec![
            ("architect".into(), "claude-opus-4-5".into()),
            ("architect".into(), "claude-opus-4-5".into()),
            ("architect".into(), "claude-sonnet-5".into()),
            ("implementer".into(), "claude-sonnet-5".into()),
            ("lookup_fast".into(), "claude-haiku-4-5".into()),
            ("lookup_fast".into(), "claude-opus-4-5".into()),
        ];
        let groups = aggregate(rows);

        // architect->architect count=2, architect->implementer count=1,
        // implementer->implementer count=1, lookup_fast->architect count=1,
        // lookup_fast->lookup_fast count=1.
        assert_eq!(groups.len(), 5);
        let architect_architect = groups
            .iter()
            .find(|g| {
                g.declared == CapabilityClass::Architect
                    && g.actual == Some(CapabilityClass::Architect)
            })
            .expect("architect->architect group");
        assert_eq!(architect_architect.count, 2);

        let lookup_fast_architect = groups
            .iter()
            .find(|g| {
                g.declared == CapabilityClass::LookupFast
                    && g.actual == Some(CapabilityClass::Architect)
            })
            .expect("lookup_fast->architect group");
        assert_eq!(lookup_fast_architect.count, 1);
    }

    #[test]
    fn aggregate_skips_unknown_declared_classes() {
        // A garbage declared value must not crash the aggregation.
        let rows = vec![
            ("garbage".into(), "claude-opus-4-5".into()),
            ("architect".into(), "claude-opus-4-5".into()),
        ];
        let groups = aggregate(rows);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].declared, CapabilityClass::Architect);
    }

    #[test]
    fn aggregate_classifies_unknown_model_as_none() {
        let rows = vec![
            ("architect".into(), "llama-3-70b".into()),
            ("implementer".into(), "claude-sonnet-5".into()),
        ];
        let groups = aggregate(rows);
        assert_eq!(groups.len(), 2);
        let unknown = groups
            .iter()
            .find(|g| g.actual.is_none())
            .expect("unknown-actual group present");
        assert_eq!(unknown.declared, CapabilityClass::Architect);
        assert_eq!(unknown.sample_model, "llama-3-70b");
    }

    #[test]
    fn render_table_shows_mismatch_marker_and_rate() {
        let groups = vec![
            Group {
                declared: CapabilityClass::Architect,
                actual: Some(CapabilityClass::Architect),
                count: 3,
                sample_model: "claude-opus-4-5".into(),
            },
            Group {
                declared: CapabilityClass::LookupFast,
                actual: Some(CapabilityClass::Architect),
                count: 1,
                sample_model: "claude-opus-4-5".into(),
            },
        ];
        let table = render_table(&groups);
        assert!(table.contains("architect"));
        assert!(table.contains("lookup_fast"));
        assert!(table.contains("← mismatch"));
        assert!(table.contains("Mismatch rate: 1 / 4 sessions (25.0%)"));
    }

    #[test]
    fn render_json_emits_well_formed_array() {
        let groups = vec![
            Group {
                declared: CapabilityClass::Implementer,
                actual: Some(CapabilityClass::Implementer),
                count: 5,
                sample_model: "claude-sonnet-5".into(),
            },
            Group {
                declared: CapabilityClass::Architect,
                actual: None,
                count: 2,
                sample_model: "llama-3-70b".into(),
            },
        ];
        let out = render_json(&groups);
        assert!(out.starts_with('['));
        assert!(out.ends_with(']'));
        assert!(out.contains("\"declared\":\"implementer\""));
        assert!(out.contains("\"actual\":\"unknown\""));
        assert!(out.contains("\"count\":5"));
    }

    #[test]
    fn class_label_matches_snake_case() {
        assert_eq!(class_label(Some(CapabilityClass::Architect)), "architect");
        assert_eq!(
            class_label(Some(CapabilityClass::Implementer)),
            "implementer"
        );
        assert_eq!(
            class_label(Some(CapabilityClass::LookupFast)),
            "lookup_fast"
        );
        assert_eq!(class_label(None), "unknown");
    }
}
