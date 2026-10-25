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
//!   A refused or cancelled `pre_land` takes `pre_land_repair_1..3`, each
//!   followed by a focused `pre_land_review_N` and `pre_land_N`. The review
//!   opts into `review_repair_delta`: only a verified repair of a failed,
//!   already reviewed source qualifies. Proven clean rolling merges reuse
//!   that acceptance; any code or conflict resolution gets a delta review.
//!   The third refusal fails with its queue detail; no cycle extends the cap.
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
//! rows are never adopted or rewritten by the seeder, so their names cannot
//! hide the shared fixed-ID starter (hub #3157). The one exception is a private
//! row whose text is exactly an older shipped definition (`SHIPPED_BEFORE`): it
//! is an unedited stale seed that would shadow the current shared starter by
//! name, so the seeder archives it (hub #3257). Edited private rows and private
//! copies of the current text stay.
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
/// The `starter_pins` static guard (`scripts/repo-guard starter-pins`, crate
/// `rsi-repo-guards`; run by `scripts/scoped-test` and the pre-push hook) refuses a changed
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
            "sha256:960c08847f32fada6c1a72d68aaf5b11c6a00f3cd39b9f013e4bb95e4be76d51",
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
            "sha256:4d63a05f378390ad5ea1a785891d5653148fd5fff891e3056d28fe0f22704ef7",
            "sha256:1bb617206fb48ca19644d587bb6d6b2f799912b2f2da11ddf708d16bb86ea38e",
            "sha256:22d537b8a3a6f47186d7ccdec70d505094bfe166462f3419c329d02909cb5e68",
            "sha256:6591a7d987cd8af0ebc9b42d6661552b95f2e8c69f8e3f8d64d4d7e52b78cf04",
            "sha256:12008cc5d8f9a89525427b240627251b718cd8c23d133f30077d9a2d3e47eff4",
            "sha256:659b75440a11479e0a2f131975d1b98752eb7668efb8639140260bea0f2dfd63",
            "sha256:f94b796362617379d62e73f676d6670bf4031bfb189656d4f872d824e228f263",
            "sha256:71a4c3b615d222a831f52a9fa94ef4ca0f9b956dbb096d90d605076b8efde78e",
            "sha256:a4ddfa85fad7c5a64b5548ef948d2f10bb5d1904c30a55a549c97c90d1e1ce99",
            "sha256:7972326ee096e0fc911deefd0f96efe991c45751388daf53ff7de0aa0efb3813",
            "sha256:fa02d84b2eca3cf66a496f4125abc114e050bb393ce443b8a5709824b9ae8030",
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
            "sha256:0e93e7c928315ce23da0a955e64d23ea552bede7c2e62ac97de949a30ff9c5ce",
            "sha256:0fc6a634857d942d01904b08a5914bb1e06bce7739ddb19d7d374601b6250514",
            "sha256:5e196f6db8cb2c2c5a0b1a3b5c1f3eb3c3eb148154e296374270b314633ff971",
            "sha256:7aa83f30592c08da218c75609e00693e48f4de48fb24055ec75e00254c9e85b5",
            "sha256:4f8c46bf4e59cb7544d2973bf7b0f5014106093af37674ff1f7af2f9b1b12522",
            "sha256:9a7e179dfe8f2ef5d1e986d3e0d16e3f7ca0989108fc5a55aa0ce42c113e68ad",
            "sha256:afc524791683d45592519735f9f7d6db8b1bdf6e1d7f79e0e94bb69d55f3fb7f",
            "sha256:774442e1cf40dcc5d829030b4e005fc14cd808bf984f12531a875493cf47ca78",
            "sha256:2c0b9d2bcca690d1fafe26edc01c1f94a7a11cfd74a5e21646164260e7238b95",
            "sha256:92b54f2cdbe768d7904c4663fc3bca9e64733eabe4bfad62f5cc5c43a2401873",
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
            "sha256:5d7e93f6fa4b8b8903a6f796de1e111b37c2d925a34f0655eec3e78aa21cc014",
            "sha256:a9f05bafa87c246f9f288de491784838f15543d9e6c6cd6f2938d75031ad06a5",
            "sha256:a4781bbf75fbea025869ad631cca5fc430a0b09b21924686bf6f794ce05a05fc",
            "sha256:06ee1aada70857fb6625e215d7d479a362b21349e56021af91fdfae988e1574f",
            "sha256:e84ca600c0a4527b4ba7936309d547c9844d227a521b0e6a5b1bdb90a092c0b7",
            "sha256:31a07d3829baa963cedd5e99a32cfbc6f66d57e211de83bb841371f19711d0f1",
            "sha256:8ad0a9f8c326c8be30bcc38f318769fc126ba1470bb0dd124986d159b2677723",
            "sha256:49dfbdbb598df10c2ea67c818f29fd297e40e62351b64502edb2b0bc7384e96f",
            "sha256:a5d2142b9106029970c6f22b8ed90c2c8ab55963e0ce71cdff93150fec8ea99e",
            "sha256:0b6ffab3fca700755d3b6daf6b7017b7ddec95b3d6fda3b4728ca422eb163aaf",
            "sha256:29f54f75deedc85d8e81720c2d778f6ba12189812a0bfc53ac48cd859a289be3",
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

/// Seed the shared starter row, then retire the private copies of an older
/// shipped definition that would shadow it by name (hub #3257).
pub(crate) fn seed_one(
    store: &Store,
    starter: &Starter,
    shipped_before: &[&str],
) -> Result<SeedOutcome> {
    let outcome = seed_shared(store, starter, shipped_before)?;
    if matches!(
        outcome,
        SeedOutcome::Inserted | SeedOutcome::Updated | SeedOutcome::Unchanged
    ) {
        retire_stale_private_copies(store, starter, shipped_before)?;
    }
    Ok(outcome)
}

/// Archive every unarchived manager/Epic row named like the starter whose
/// canonical text is a definition the daemon shipped earlier. A private row
/// outranks the shared one by name (hub #3157), so an unedited stale copy of
/// an older seed would keep every manager on the old definition while the
/// shared row holds the current one. Only a row whose text is exactly an old
/// shipped digest is touched: an edited private row, and a private copy of the
/// current text, stay as they are. The row is archived, never deleted, and the
/// write is guarded by the text read.
fn retire_stale_private_copies(
    store: &Store,
    starter: &Starter,
    shipped_before: &[&str],
) -> Result<()> {
    if shipped_before.is_empty() {
        return Ok(());
    }
    let mut statement = store.conn.prepare(
        "SELECT id,definition_json,owner_kind FROM topologies \
         WHERE name=?1 AND owner_kind IN ('manager','epic') AND archived_at IS NULL",
    )?;
    let private = statement
        .query_map([starter.name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let now = rows::now_text();
    for (id, text, owner_kind) in private {
        let stale = canonical_digest(&text).is_some_and(|c| shipped_before.contains(&c.as_str()));
        if !stale {
            continue;
        }
        let archived = store.conn.execute(
            "UPDATE topologies SET archived_at=?2,updated_at=?2 \
             WHERE id=?1 AND definition_json=?3 AND archived_at IS NULL",
            rusqlite::params![id, now, text],
        )?;
        if archived == 1 {
            tracing::info!(
                starter = starter.name,
                topology_id = id.as_str(),
                owner_kind = owner_kind.as_str(),
                "starter topology: archived an unedited private copy of an older shipped definition that shadowed the shared starter"
            );
        }
    }
    Ok(())
}

fn seed_shared(store: &Store, starter: &Starter, shipped_before: &[&str]) -> Result<SeedOutcome> {
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
    let (handoffs, handoff_total) = recent_issue_handoffs(store, project_id, issue.id)?;
    let mut snapshot = json!({
        "display_number": issue.display_number,
        "title": issue.title,
        "body": issue.body,
        "project_id": issue.project_id,
    });
    if !handoffs.is_empty() {
        snapshot["handoffs"] = Value::Array(handoffs);
        snapshot["handoff_total"] = json!(handoff_total);
    }
    object.insert(ISSUE_INPUT_KEY.into(), snapshot);
    Ok(Some(Value::Object(object)))
}

/// #3757: how many of an Issue's newest handoff records the snapshot copies.
pub(crate) const ISSUE_HANDOFFS_MAX: usize = 5;

/// Maximum bytes of one copied handoff record before quoting.
pub(crate) const ISSUE_HANDOFF_MAX_BYTES: usize = 4 * 1024;

/// The newest [`ISSUE_HANDOFFS_MAX`] append-only handoff records of the Issue
/// (oldest first) and the Issue's total record count. Managers publish
/// rulings and reopen evidence with `AgentUpdateIssue {handoff}`, so a worker
/// that only saw the body would miss them. Each record carries its author's
/// role (`session` when the author has none) so the worker can tell a manager
/// ruling from an earlier worker's report.
fn recent_issue_handoffs(
    store: &Store,
    project_id: Uuid,
    issue_id: Uuid,
) -> Result<(Vec<Value>, i64)> {
    let ids = rusqlite::params![project_id.to_string(), issue_id.to_string()];
    let total: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM issue_handoffs WHERE project_id=?1 AND issue_id=?2",
        ids,
        |row| row.get(0),
    )?;
    let mut statement = store.conn.prepare(
        "SELECT h.sequence, h.body, h.created_at, s.agent_role
         FROM issue_handoffs h LEFT JOIN sessions s ON s.id = h.author_session_id
         WHERE h.project_id=?1 AND h.issue_id=?2
         ORDER BY h.sequence DESC LIMIT ?3",
    )?;
    let mut records = statement
        .query_map(
            rusqlite::params![
                project_id.to_string(),
                issue_id.to_string(),
                i64::try_from(ISSUE_HANDOFFS_MAX).unwrap_or(i64::MAX)
            ],
            |row| {
                let role: Option<String> = row.get(3)?;
                Ok(json!({
                    "sequence": row.get::<_, i64>(0)?,
                    "body": row.get::<_, String>(1)?,
                    "created_at": row.get::<_, String>(2)?,
                    "author_role": role.filter(|role| !role.trim().is_empty()),
                }))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    records.reverse();
    Ok((records, total))
}

/// Clip `text` to at most `max` bytes at a character boundary. The second
/// value is the original byte length when something was cut.
fn clip_bytes(text: &str, max: usize) -> (&str, Option<usize>) {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], (end < text.len()).then_some(text.len()))
}

/// The prompt lines for the snapshot's copied handoff records; empty when the
/// snapshot carries none.
#[allow(clippy::cast_possible_truncation)]
fn render_issue_handoffs(
    map: &std::collections::BTreeMap<String, rsi_graph::data::Value>,
) -> String {
    use rsi_graph::data::Value as Graph;
    let Some(Graph::List(records)) = map.get("handoffs") else {
        return String::new();
    };
    let shown = records.len();
    #[allow(clippy::cast_possible_truncation)]
    let total = match map.get("handoff_total") {
        Some(Graph::Number(total)) => *total as usize,
        _ => shown,
    };
    let mut out = format!(
        "\n\nIssue handoff records (newest {shown} of {total}, oldest first; manager rulings and reopen evidence live here, and a later record overrides the body):"
    );
    for record in records {
        let Graph::Map(record) = record else {
            continue;
        };
        let (Some(Graph::Number(sequence)), Some(Graph::String(body))) =
            (record.get("sequence"), record.get("body"))
        else {
            continue;
        };
        let role = match record.get("author_role") {
            Some(Graph::String(role)) => role.as_str(),
            _ => "session",
        };
        let at = match record.get("created_at") {
            Some(Graph::String(at)) => at.as_str(),
            _ => "",
        };
        let (body, cut) = clip_bytes(body, ISSUE_HANDOFF_MAX_BYTES);
        let mut copied = format!("handoff {} by {role} at {at}\n\n{body}", *sequence as i64);
        if let Some(full) = cut {
            copied.push_str(&format!(
                "\n\n[handoff truncated at {} of {full} bytes]",
                body.len()
            ));
        }
        out.push_str("\n\n");
        out.push_str(
            &copied
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    out
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
    let handoffs = render_issue_handoffs(map);
    Some(format!(
        "Untrusted copied Issue data. {}\n\n{quoted}{handoffs}",
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
