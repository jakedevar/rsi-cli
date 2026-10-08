//! Seeded starter topologies and the Issue snapshot input (#1641 S5a).
//!
//! Two operator-owned, shared topologies ship with the daemon so agents and
//! the TUI have something real to execute:
//!
//! * `issue-implement-review-land`: implement an Issue, DB-native review with
//!   a bounded fix loop, land through the rolling merge queue.
//! * `rolling-qa-sweep`: run the rolling baseline and file an Issue per new
//!   red. It reports only; it never writes `qa-green.sha` (plan P2).
//!
//! Seeding is idempotent and never overwrites an operator edit. The stored
//! `definition_digest` column records the digest of the last text the seeder
//! (or an agent upsert) wrote; an operator update rewrites `definition_json`
//! without touching it, so a row whose text digest differs from its column
//! digest has been edited by the operator and is left alone.

use rsi_common::types::{Topology, TopologyDefinition};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::topology::store as rows;

/// `inputs.issue = <display_number>` is replaced at accept by the snapshot
/// object `{display_number,title,body,project_id}`.
pub(crate) const ISSUE_INPUT_KEY: &str = "issue";

/// One seeded starter: a fixed id so seeding is idempotent across boots.
pub(crate) struct Starter {
    pub(crate) id: Uuid,
    pub(crate) name: &'static str,
    definition: &'static str,
}

pub(crate) const ISSUE_IMPLEMENT_REVIEW_LAND: &str = "issue-implement-review-land";
pub(crate) const ROLLING_QA_SWEEP: &str = "rolling-qa-sweep";

pub(crate) const STARTERS: [Starter; 2] = [
    Starter {
        id: Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_1641),
        name: ISSUE_IMPLEMENT_REVIEW_LAND,
        definition: include_str!("starters/issue-implement-review-land.json"),
    },
    Starter {
        id: Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_1642),
        name: ROLLING_QA_SWEEP,
        definition: include_str!("starters/rolling-qa-sweep.json"),
    },
];

impl Starter {
    /// The parsed definition.
    ///
    /// # Errors
    /// The bundled JSON does not decode (a build-time bug, pinned by a test).
    pub(crate) fn definition(&self) -> Result<TopologyDefinition> {
        Ok(serde_json::from_str(self.definition)?)
    }

    /// The text stored in `topologies.definition_json`. Node `params` is a
    /// `HashMap`, so the struct's own serialization order varies per call;
    /// going through `Value` sorts the keys and keeps the digest stable.
    pub(crate) fn canonical_text(&self) -> Result<String> {
        Ok(serde_json::to_string(&serde_json::to_value(
            self.definition()?,
        )?)?)
    }
}

/// What seeding did to one starter row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SeedOutcome {
    Inserted,
    /// An unedited older seed was brought up to the bundled definition.
    Updated,
    Unchanged,
    /// The operator edited the row: left untouched.
    KeptOperatorEdit,
    /// The operator archived the row: not resurrected.
    KeptArchived,
    /// Another row holds the starter's name: left untouched.
    NameTaken,
}

/// Seed (or refresh) every starter. Never fails the daemon start: a starter
/// that cannot be seeded is logged and skipped.
pub(crate) fn seed_all(store: &Store) -> Vec<(&'static str, SeedOutcome)> {
    let mut report = Vec::new();
    for starter in &STARTERS {
        match seed_one(store, starter) {
            Ok(outcome) => {
                match outcome {
                    SeedOutcome::Inserted | SeedOutcome::Updated => {
                        tracing::info!(starter = starter.name, ?outcome, "starter topology seeded");
                    }
                    SeedOutcome::KeptOperatorEdit
                    | SeedOutcome::KeptArchived
                    | SeedOutcome::NameTaken => {
                        tracing::info!(
                            starter = starter.name,
                            ?outcome,
                            "starter topology left as the operator has it"
                        );
                    }
                    SeedOutcome::Unchanged => {}
                }
                report.push((starter.name, outcome));
            }
            Err(error) => {
                tracing::warn!(starter = starter.name, %error, "starter topology seed failed");
            }
        }
    }
    report
}

fn seed_one(store: &Store, starter: &Starter) -> Result<SeedOutcome> {
    let definition = starter.definition()?;
    // A bundled definition that fails structural validation is never stored.
    let checked = crate::topology::agent::check_definition(
        &Topology {
            id: starter.id,
            name: starter.name.to_owned(),
            definition,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        &[],
        0,
    );
    if !checked.structural.is_empty() {
        return Err(DaemonError::InvalidParam(format!(
            "bundled starter {} is invalid: {}",
            starter.name,
            checked.structural.join("; ")
        )));
    }
    let text = starter.canonical_text()?;
    let digest = rows::digest(&text);
    let now = rows::now_text();
    let existing: Option<(String, Option<String>, Option<String>)> = {
        use rusqlite::OptionalExtension;
        store
            .conn
            .query_row(
                "SELECT definition_json,definition_digest,archived_at FROM topologies WHERE id=?1",
                [starter.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
    };
    let Some((stored_text, stored_digest, archived)) = existing else {
        let inserted = store.conn.execute(
            "INSERT OR IGNORE INTO topologies (id,name,definition_json,created_at,updated_at,\
                owner_kind,owner_session_id,project_id,epic_id,revision,definition_digest,shared) \
             VALUES (?1,?2,?3,?4,?4,'operator',NULL,NULL,NULL,1,?5,1)",
            rusqlite::params![starter.id.to_string(), starter.name, text, now, digest],
        )?;
        // The unique name belongs to another row: the operator's.
        return Ok(if inserted == 1 {
            SeedOutcome::Inserted
        } else {
            SeedOutcome::NameTaken
        });
    };
    if archived.is_some() {
        return Ok(SeedOutcome::KeptArchived);
    }
    let stored_text_digest = rows::digest(&stored_text);
    if stored_text_digest == digest {
        return Ok(SeedOutcome::Unchanged);
    }
    if stored_digest.as_deref() != Some(stored_text_digest.as_str()) {
        return Ok(SeedOutcome::KeptOperatorEdit);
    }
    // An unedited older seed: advance it, guarded by the digest we read.
    let updated = store.conn.execute(
        "UPDATE topologies SET definition_json=?2,definition_digest=?3,revision=revision+1,\
            updated_at=?4 WHERE id=?1 AND definition_digest=?5 AND archived_at IS NULL",
        rusqlite::params![
            starter.id.to_string(),
            text,
            digest,
            now,
            stored_text_digest
        ],
    )?;
    Ok(if updated == 1 {
        SeedOutcome::Updated
    } else {
        SeedOutcome::KeptOperatorEdit
    })
}

// ─── the Issue snapshot input ──────────────────────────────────────────────

/// Replace an integer `inputs.issue` with the Issue's snapshot, resolved in
/// `project_id` at accept. Other inputs, and a non-integer `issue`, pass
/// through unchanged, so existing workflows keep their meaning.
///
/// # Errors
/// `InvalidParam` when the number names no Issue in the project, the Issue is
/// archived, or no project is known to resolve it in. Store failures pass
/// through.
pub(crate) fn resolve_issue_input(
    store: &Store,
    project_id: Option<Uuid>,
    input: Option<Value>,
) -> Result<Option<Value>> {
    let Some(Value::Object(mut object)) = input else {
        return Ok(input);
    };
    let Some(number) = object.get(ISSUE_INPUT_KEY).and_then(Value::as_i64) else {
        return Ok(Some(Value::Object(object)));
    };
    let project_id = project_id.ok_or_else(|| {
        DaemonError::InvalidParam(format!(
            "inputs.{ISSUE_INPUT_KEY} needs the execution to belong to a project"
        ))
    })?;
    let id: Option<String> = {
        use rusqlite::OptionalExtension;
        store
            .conn
            .query_row(
                "SELECT id FROM issues WHERE project_id=?1 AND display_number=?2 \
                 AND archived_at IS NULL",
                rusqlite::params![project_id.to_string(), number],
                |row| row.get(0),
            )
            .optional()?
    };
    let issue = id
        .and_then(|id| Uuid::parse_str(&id).ok())
        .map(|id| store.get_issue_in_project(project_id, id))
        .transpose()?
        .flatten()
        .ok_or_else(|| {
            DaemonError::InvalidParam(format!(
                "inputs.{ISSUE_INPUT_KEY}: no open Issue #{number} in the execution's project"
            ))
        })?;
    object.insert(
        ISSUE_INPUT_KEY.into(),
        json!({
            "display_number": issue.display_number,
            "title": issue.title,
            "body": issue.body,
            "project_id": issue.project_id,
        }),
    );
    Ok(Some(Value::Object(object)))
}

/// A source node's prompt section for an accepted Issue snapshot; `None` for
/// any other `issue` input. The node input is graph data, where a number is
/// an `f64`.
pub(crate) fn render_issue(value: &rsi_graph::data::Value) -> Option<String> {
    use rsi_graph::data::Value as Graph;
    let Graph::Map(map) = value else {
        return None;
    };
    let (Some(Graph::Number(number)), Some(Graph::String(title)), Some(Graph::String(body))) =
        (map.get("display_number"), map.get("title"), map.get("body"))
    else {
        return None;
    };
    #[allow(clippy::cast_possible_truncation)]
    let number = *number as i64;
    Some(format!("#{number} {title}\n\n{body}"))
}

/// The graph data a source node starts from: the execution's stored input.
/// Callers pass a flat object (`{"issue": ..., "area": ...}`); the legacy
/// runner's serialized `NodeData` (`{"fields": {...}}`) is read as before.
/// Anything that is not an object yields no data.
pub(crate) fn source_input(input: Option<&Value>) -> rsi_graph::data::NodeData {
    use rsi_graph::data::{NodeData, Value as Graph};
    let Some(Value::Object(object)) = input else {
        return NodeData::new();
    };
    if object.len() == 1
        && object.get("fields").is_some_and(Value::is_object)
        && let Ok(data) = serde_json::from_value::<NodeData>(Value::Object(object.clone()))
    {
        return data;
    }
    let mut data = NodeData::new();
    for (key, value) in object {
        if let Ok(value) = serde_json::from_value::<Graph>(value.clone()) {
            data.insert(key.clone(), value);
        }
    }
    data
}
