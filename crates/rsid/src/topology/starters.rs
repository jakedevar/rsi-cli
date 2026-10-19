//! Seeded starter topologies and the Issue snapshot input (#1641 S5a).
//!
//! Operator-owned, shared topologies ship with the daemon so agents and the
//! TUI have something real to execute:
//!
//! * `issue-pipeline` (#2304): the ordinary default. One implementation worker
//!   verifies with its own final-HEAD `scoped_test` job, the daemon lands the
//!   verified commit with derived filters and closes the Issue with the
//!   published SHA; a refusal takes a verified repair, up to three (#2892). A land the daemon
//!   refuses `land_requires_review` (the worker's change adds a migration, or
//!   another change that needs a pre-land review) is not a stranded run
//!   (#2688): its `review_required` edge routes the exact head to the same
//!   execution's own `pre_review` node (the review node of
//!   `issue-pipeline-reviewed`), `pre_fix` repairs changes requested within the
//!   loop bound, and `pre_land` lands what the reviewer accepted. A repaired
//!   head the queue refuses `land_requires_review` takes the same route out of
//!   `land_repair` (#2699): `repair_review` reviews the exact repair head,
//!   `repair_fix` fixes within the loop bound, `repair_land` lands what was accepted.
//!   A repaired head the queue refuses again (rolling advanced in the same file
//!   while it waited, #2892) is repaired again by `repair_2` -> `land_repair_2`,
//!   then `repair_3` -> `land_repair_3`: three repairs in all, each its own
//!   reported node, the run failing only when the third is refused or a repair
//!   worker reports a conflict that needs a decision.
//! * `issue-pipeline-reviewed` (#2565): the pre-merge route. An accepted review
//!   by the other model family of the exact verified head precedes every
//!   enqueue, including a repaired source; for migrations and credential, IAM
//!   or network-exposure changes.
//! * `issue-pipeline-post-review` (#2565): the post-land route. The ordinary
//!   pipeline (the same three-repair chain, #2899) plus one review of whatever
//!   landed, the head of whichever repair landed included
//!   (`post_review_repair`, `_2`, `_3`); changes requested become deduped
//!   follow-up Issues and never reopen the closed Issue.
//! * `issue-implement-review-land`: implement an Issue, DB-native review with
//!   a bounded fix loop, land through the rolling merge queue.
//! * `rolling-qa-sweep`: run the rolling baseline and file an Issue per new
//!   red. It reports only; it never writes `qa-green.sha` (plan P2).
//!
//! Seeding is idempotent and never overwrites an edit. A row (found by the
//! starter's fixed id, else by its operator-owned name) follows the shipped
//! definition when its canonical digest is one the daemon shipped earlier
//! (`SHIPPED_BEFORE`), or when it is still the text the seeder wrote: the stored
//! `definition_digest` column records the digest of the last text the seeder
//! wrote, and an operator update rewrites `definition_json` without touching
//! it. Any other row is kept, logged `starter_drift` with both digests and
//! flagged `starter_drift` on `AgentTopologyList` (#2660). Private manager/Epic
//! rows are never adopted by the seeder, so their names cannot hide the shared
//! fixed-ID starter and their definitions remain untouched (hub #3157).
//!
//! Every topology digest is taken over the canonical (key-sorted) definition
//! text (`store::definition_digest`, #2693), so `validate_only`, the stored
//! column, `AgentTopologyList`, `expected_digest`, `starter_drift` and the
//! `SHIPPED_BEFORE` pins agree for one definition whoever wrote the row.

use rsi_common::topology_agent::StarterDriftV1;
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
pub(crate) const ISSUE_PIPELINE: &str = "issue-pipeline";

pub(crate) const ISSUE_PIPELINE_REVIEWED: &str = "issue-pipeline-reviewed";
pub(crate) const ISSUE_PIPELINE_POST_REVIEW: &str = "issue-pipeline-post-review";

pub(crate) const STARTERS: [Starter; 5] = [
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
    Starter {
        id: Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_2304),
        name: ISSUE_PIPELINE,
        definition: include_str!("starters/issue-pipeline.json"),
    },
    Starter {
        id: Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_2565),
        name: ISSUE_PIPELINE_REVIEWED,
        definition: include_str!("starters/issue-pipeline-reviewed.json"),
    },
    Starter {
        id: Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_2566),
        name: ISSUE_PIPELINE_POST_REVIEW,
        definition: include_str!("starters/issue-pipeline-post-review.json"),
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
        rows::canonical_definition_text(&self.definition()?)
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
        match seed_one(store, starter, shipped_before(starter.name)) {
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

/// The canonical digest of definition text, however its object keys were
/// ordered when it was written: an agent upsert serializes `params` in
/// `HashMap` order, the seeder in sorted order. `None` when the text is not a
/// definition this build can read.
pub(crate) fn canonical_digest(text: &str) -> Option<String> {
    rows::canonical_digest(text)
}

/// Canonical digests of every definition the daemon shipped for a starter
/// before the current one (#2660). A stored row that holds one of them has not
/// been edited since the daemon shipped it. Operator seed rows follow the
/// shipped definition; private manager/Epic rows are left untouched. A change
/// to a starter's JSON appends the digest it replaces here (the value its test pin held) and moves that pin in `starters_tests.rs`
/// to the new canonical digest (the failing fixture test prints it).
/// `tools/check_starter_pins.py` (the `starter_pins` static guard of
/// `scripts/scoped-test`, also run by the pre-push hook) refuses a changed
/// starter JSON whose two pins did not move together, before any test runs
/// (#3070).
pub(crate) const SHIPPED_BEFORE: &[(&str, &[&str])] = &[
    (
        ISSUE_IMPLEMENT_REVIEW_LAND,
        &[
            "sha256:07a3b240ff112a990dc84b160620903de7212c4a5d295596395903de436906cd",
            "sha256:94adb69b21b20328d9aa502489562902310b250e3793553736114c306178ced0",
            "sha256:65f9c4dffe623fd578bc5ce81786c0ab64f0590b583d2759a2278e7fec1daa6d",
            "sha256:afc8726c358fee100f0da6a7a56dadadccbae4d6cc721f3f584e101b347c96cf",
            "sha256:61013886be5913c6923de5df843ad6f099b28f4d389c3d9d1da301e8d7ebb442",
        ],
    ),
    (
        ROLLING_QA_SWEEP,
        &["sha256:f1b0d68e88255fbf0016bf4382b89f5d299b490903bd36802e766e3f54bf066c"],
    ),
    (
        ISSUE_PIPELINE,
        &[
            "sha256:bc7d4f3684ff2913c2531498a7c70a02a6754859caa02f4a0fa8cde28fb39810",
            "sha256:f90c7f4e4adf4b7cfacf4a13bdbc67524c3ca01d4af65d5d2f0ce7e0bd03419a",
            "sha256:6c8f39f4c5f45954e247e5a7be5440a4456a06830fe25bea93e7a287180e8975",
            "sha256:a4496afe37568e6b27da40afd9fe2132679205495374d65c1a79227d35dcbeea",
            "sha256:b7fe38db99f6f88ed1d842c50538a1d29382bf7263e39a402c3d19493c13aa0f",
            "sha256:5f9fa5f603819627d4c786bac36528b43ad5f5897ace4f999b076356a7a60868",
            "sha256:4d65b01f2d56950870db728ec930bafc911fabcad2921ed74b9a2ad1e9163d0d",
            "sha256:beece521a509608557056ab773588e8d6a67b71c85933e33af49053d34ca85b7",
            "sha256:e7039d2be6ef42a12aaf009a98f4436fe1003badd4a50e8dd9a030f5678bd37b",
        ],
    ),
    (
        ISSUE_PIPELINE_REVIEWED,
        &[
            "sha256:3fa558b5cbe39df35fd9d624b7042dc9b437c4b737414632f3511928374103d0",
            "sha256:75616f4020b5046e532ab478663d3e5065dc951ca8c7872149b8d21c7d62029d",
            "sha256:842b39539c1e75dee1f6ebcbc2b869bf6659e27bd0f8510ea6bf954a0c4c4488",
            "sha256:d0135a0322249d4e3f30cb06c652eb03ff11498bdf74db00f6a40f232a14580e",
            "sha256:b8a085a9f457d9f194a2ac14604b92b6278d8abf944ebcfb70e633be0433f7b0",
            "sha256:d78517e1e45ce25dd1560c855960bf0831268e6ebb1837fc901ec3f81e276f0e",
        ],
    ),
    (
        ISSUE_PIPELINE_POST_REVIEW,
        &[
            "sha256:1013e0dff7e180d7fd3cadaec7a8c55bf6500db9594d24501bfe02796214a41f",
            "sha256:f029390671b2021908fa9a5bf6d5c2d7197a5684378c6881175d84bd4eab3bef",
            "sha256:e0f7d24cce6c3660544f87b8780e45a289a9120c251c8bce05edacfff655cc1e",
            "sha256:b01fce61612548e969cc5b331efeeeb7ae61b099030d792c7b260e62d72eccbd",
            "sha256:69760bd2114280c124aa7202396204ce422bb213a254dcc03fd496fe1e6f61d8",
            "sha256:502287cd2dffc755f7bd920bcffd0f8d48dec97593cc0e4d49c79cd7a030b0d1",
            "sha256:9b687e0deaecdfd0ea285dc9f7901bfc1524a6f851c2230c2d45293c64285fe9",
            "sha256:44d6891f6152a16a800f303542f28da4f8d137700db784f4d2e8630b1d0549e3",
            "sha256:6ae9e409915117c5503d8abc6d79ea774c97e5aeec9cfc77c353fa317e2a37ed",
            "sha256:2d6e599f8ec296ff8fe818c1ce915b9a22f56935f83414f6d6073911defd64bd",
            "sha256:c88c506c0b28adc9ed767c35c4dc3ca9ea06f155db6e8a998a5931f8c469cc66",
        ],
    ),
];

fn shipped_before(name: &str) -> &'static [&'static str] {
    SHIPPED_BEFORE
        .iter()
        .find(|(starter, _)| *starter == name)
        .map_or(&[], |(_, digests)| digests)
}

/// A stored definition named like a starter that is not the shipped one
/// (#2660): someone edited it, or it is an older shipped text this build does
/// not upgrade.
#[must_use]
pub(crate) fn drift(name: &str, stored_text: &str) -> Option<StarterDriftV1> {
    let starter = STARTERS.iter().find(|starter| starter.name == name)?;
    let shipped = rows::digest(&starter.canonical_text().ok()?);
    let stored = rows::definition_digest(stored_text);
    (stored != shipped).then_some(StarterDriftV1 {
        shipped_digest: shipped,
    })
}

struct StarterRow {
    id: String,
    /// The row holds the starter's fixed id (the seeder wrote it).
    by_id: bool,
    text: String,
    column_digest: Option<String>,
    archived: bool,
}

/// The row holding the starter: its fixed id, else the row holding its name
/// in the operator namespace. Private manager/Epic rows are never adopted or
/// refreshed, even when their text matches an older shipped definition.
fn find_row(store: &Store, starter: &Starter) -> Result<Option<StarterRow>> {
    use rusqlite::OptionalExtension;
    let read = |by_id: bool| {
        move |row: &rusqlite::Row<'_>| -> rusqlite::Result<StarterRow> {
            Ok(StarterRow {
                id: row.get(0)?,
                by_id,
                text: row.get(1)?,
                column_digest: row.get(2)?,
                archived: row.get::<_, Option<String>>(3)?.is_some(),
            })
        }
    };
    let by_id = store
        .conn
        .query_row(
            "SELECT id,definition_json,definition_digest,archived_at FROM topologies WHERE id=?1 AND owner_kind='operator'",
            [starter.id.to_string()],
            read(true),
        )
        .optional()?;
    if by_id.is_some() {
        return Ok(by_id);
    }
    Ok(store
        .conn
        .query_row(
            "SELECT id,definition_json,definition_digest,archived_at FROM topologies \
             WHERE name=?1 AND owner_kind='operator' AND project_id IS NULL AND epic_id IS NULL",
            [starter.name],
            read(false),
        )
        .optional()?)
}

pub(crate) fn seed_one(
    store: &Store,
    starter: &Starter,
    shipped_before: &[&str],
) -> Result<SeedOutcome> {
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
    let Some(row) = find_row(store, starter)? else {
        let inserted = store.conn.execute(
            "INSERT OR IGNORE INTO topologies (id,name,definition_json,created_at,updated_at,\
                owner_kind,owner_session_id,project_id,epic_id,revision,definition_digest,shared) \
             VALUES (?1,?2,?3,?4,?4,'operator',NULL,NULL,NULL,1,?5,1)",
            rusqlite::params![starter.id.to_string(), starter.name, text, now, digest],
        )?;
        // An operator row can still reserve an archived/custom starter name.
        return Ok(if inserted == 1 {
            SeedOutcome::Inserted
        } else {
            SeedOutcome::NameTaken
        });
    };
    if row.archived {
        return Ok(SeedOutcome::KeptArchived);
    }
    let stored_text_digest = rows::digest(&row.text);
    if stored_text_digest == digest {
        return Ok(SeedOutcome::Unchanged);
    }
    let canonical = canonical_digest(&row.text);
    if canonical.as_deref() == Some(digest.as_str()) {
        // The shipped definition with its keys in another order.
        return Ok(SeedOutcome::Unchanged);
    }
    // Unedited since a seed (the column still names the text), or a text the
    // daemon shipped earlier: advance it, guarded by the text we read.
    let seeded_unedited =
        row.by_id && row.column_digest.as_deref() == Some(stored_text_digest.as_str());
    let shipped_earlier = canonical
        .as_deref()
        .is_some_and(|c| shipped_before.contains(&c));
    if seeded_unedited || shipped_earlier {
        let updated = store.conn.execute(
            "UPDATE topologies SET definition_json=?2,definition_digest=?3,revision=revision+1,\
                updated_at=?4 WHERE id=?1 AND definition_json=?5 AND archived_at IS NULL",
            rusqlite::params![row.id, text, digest, now, row.text],
        )?;
        if updated == 1 {
            return Ok(SeedOutcome::Updated);
        }
    }
    tracing::warn!(
        starter = starter.name,
        stored_digest = canonical.as_deref().unwrap_or(stored_text_digest.as_str()),
        shipped_digest = digest.as_str(),
        "starter_drift: the stored topology differs from the shipped definition and is kept"
    );
    Ok(if row.by_id {
        SeedOutcome::KeptOperatorEdit
    } else {
        SeedOutcome::NameTaken
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

/// The Issue an accepted execution input names: `(project_id, display_number)`
/// of the `inputs.issue` snapshot [`resolve_issue_input`] wrote. `None` when
/// the input carries no snapshot (absent, or a value that was not an integer).
pub(crate) fn issue_snapshot(input: Option<&Value>) -> Option<(Uuid, i64)> {
    let issue = input?.get(ISSUE_INPUT_KEY)?;
    let number = issue.get("display_number")?.as_i64()?;
    let project = Uuid::parse_str(issue.get("project_id")?.as_str()?).ok()?;
    Some((project, number))
}

/// #2260: a topology with a `close_issue` land closes `inputs.issue` when it
/// publishes, so an execution accepted without that Issue snapshot is refused
/// up front (`issue_input_required`) instead of landing and leaving the Issue
/// open.
///
/// # Errors
/// The typed `issue_input_required` refusal.
pub(crate) fn require_issue_input(
    steps: &crate::topology::steps::WorkflowSteps,
    input: Option<&Value>,
) -> Result<()> {
    if steps.has_close_issue() && issue_snapshot(input).is_none() {
        return Err(crate::topology::agent::refusal(
            "issue_input_required",
            "this topology closes its input Issue when it lands: pass inputs.issue as the display number of an open Issue in the Epic's project",
        ));
    }
    Ok(())
}

/// Maximum copied Issue body bytes per node query, before quoting.
pub(crate) const ISSUE_BODY_MAX_BYTES: usize = 16 * 1024;

/// A node's prompt section for an accepted Issue snapshot; `None` for
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
    let mut end = body.len().min(ISSUE_BODY_MAX_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = if end < body.len() {
        format!(
            "\n\n[Issue body truncated at {end} of {} bytes]",
            body.len()
        )
    } else {
        String::new()
    };
    let copied = format!("#{number} {title}\n\n{}{truncated}", &body[..end]);
    let quoted = copied
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Untrusted copied Issue data. {}\n\n{quoted}",
        rsi_common::daemon_handoff::UNTRUSTED_TEXT_NOTICE
    ))
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
