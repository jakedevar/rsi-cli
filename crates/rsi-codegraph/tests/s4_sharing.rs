#![allow(clippy::expect_used, clippy::unwrap_used)] // Exact temporary database fixtures.
//! #2245: facts are stored once per distinct file content. A generation is a
//! manifest of shared blocks, so repeated generations and extra workspaces at the
//! same content add manifest rows, not copies of the graph.

use std::sync::OnceLock;

use rsi_codegraph::invalidate::ExtractionCache;
use rsi_codegraph::query::{QueryFilter, QueryLimits, SearchMode, SnapshotSelector};
use rsi_codegraph::{
    CodegraphStore, ExtractionContract, ExtractionMode, ExtractorIdentity, PerFileFacts,
    PublishFault, ReadySnapshot, SourceFile, SourceVersion, StagedExtraction, WorkspaceInstanceKey,
    WorkspaceScope,
};
use rusqlite::{Connection, params};
use uuid::Uuid;

const PROJECT: Uuid = Uuid::from_u128(0x2245_2245_2245_4245_8245_2245_2245_2245);
const FILES: usize = 200;
const FUNCTIONS: usize = 12;
const GENERATIONS: usize = 20;

fn manifest() -> StagedExtraction {
    StagedExtraction {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedInferredV1_1,
            extractor: ExtractorIdentity {
                name: "s4-sharing".into(),
                version: "1".into(),
            },
        },
        grammar_version: "grammar-1".into(),
        rule_version: "rules-1".into(),
        normalization_version: "normalization-1".into(),
        config_digest: "config-1".into(),
    }
}

/// One module of the synthetic crate. Each function calls its neighbour, a
/// function in the next module (a cross-file edge), and std items that stay
/// unresolved, so a file carries nodes, structure, resolved edges and sites.
fn module_source(index: usize, revision: usize) -> String {
    let next = (index + 1) % FILES;
    let mut text = String::new();
    for function in 0..FUNCTIONS {
        let neighbour = (function + 1) % FUNCTIONS;
        text.push_str(&format!(
            "pub fn f{function}(x: u32) -> u32 {{\n    let mut v: Vec<u32> = Vec::new();\n    v.push(x);\n    let a = f{neighbour}(x);\n    let b = crate::m{next:03}::f{function}(x);\n    a + b + v.len() as u32\n}}\n\n"
        ));
    }
    if revision > 0 {
        text.push_str(&format!("pub fn added_{revision}() {{}}\n"));
    }
    text
}

/// The corpus at generation `generation` (1-based): module `g - 1` changes in
/// generation `g`, so every generation differs from the last in exactly one file.
fn corpus(generation: usize) -> Vec<SourceFile> {
    let mut files = vec![
        SourceFile {
            relative_path: "Cargo.toml".into(),
            bytes: b"[package]\nname = \"synthetic\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"
                .to_vec(),
        },
        SourceFile {
            relative_path: "src/lib.rs".into(),
            bytes: (0..FILES)
                .map(|index| format!("pub mod m{index:03};\n"))
                .collect::<String>()
                .into_bytes(),
        },
    ];
    for index in 0..FILES {
        let revision = usize::from(index >= 1 && index < generation);
        files.push(SourceFile {
            relative_path: format!("src/m{index:03}.rs"),
            bytes: module_source(index, revision).into_bytes(),
        });
    }
    files
}

/// Resolved per-file facts for every generation, built once for all tests.
fn generations() -> &'static Vec<Vec<PerFileFacts>> {
    static FACTS: OnceLock<Vec<Vec<PerFileFacts>>> = OnceLock::new();
    FACTS.get_or_init(|| {
        (1..=GENERATIONS)
            .map(|generation| {
                ExtractionCache::new()
                    .update(corpus(generation))
                    .unwrap()
                    .facts
            })
            .collect()
    })
}

fn publish(
    store: &mut CodegraphStore,
    scope: &WorkspaceScope,
    facts: &[PerFileFacts],
) -> ReadySnapshot {
    let owners = facts
        .iter()
        .map(|file| SourceVersion {
            relative_path: file.file.relative_path.clone(),
            source_digest: blake3::hash(&file.file.bytes).to_hex().to_string(),
        })
        .collect::<Vec<_>>();
    let run = store.begin_staged(scope, &manifest(), &owners).unwrap();
    for file in facts {
        store.stage_file(&run, file).unwrap();
    }
    store.publish_staged(&run, PublishFault::None).unwrap()
}

fn open(directory: &tempfile::TempDir) -> CodegraphStore {
    CodegraphStore::open(directory.path().join("graph.sqlite"), PROJECT).unwrap()
}

fn sandbox(index: u128) -> WorkspaceInstanceKey {
    WorkspaceInstanceKey::RsiSandbox(Uuid::from_u128(index))
}

/// The v8 fact tables, as the compatibility views present them: every row of one
/// generation without its workspace and generation columns, in a stable order.
fn fact_rows(
    connection: &Connection,
    workspace: Uuid,
    generation: i64,
) -> Vec<(String, Vec<String>)> {
    [
        "cg_files",
        "cg_file_diagnostics",
        "cg_nodes",
        "cg_relations",
        "cg_evidence",
        "cg_unresolved_references",
    ]
    .into_iter()
    .map(|table| {
        let mut statement = connection
            .prepare(&format!(
                "SELECT * FROM {table} WHERE workspace_id=?1 AND generation=?2"
            ))
            .unwrap();
        let columns = statement.column_count();
        let mut rows = statement
            .query_map(params![workspace.to_string(), generation], |row| {
                (2..columns)
                    .map(|column| {
                        row.get::<_, rusqlite::types::Value>(column)
                            .map(|value| format!("{value:?}"))
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(|values| values.join("|"))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.sort();
        (table.to_owned(), rows)
    })
    .collect()
}

fn file_size(directory: &tempfile::TempDir) -> u64 {
    std::fs::metadata(directory.path().join("graph.sqlite"))
        .unwrap()
        .len()
}

#[test]
fn twenty_generations_each_changing_one_file_grow_the_database_by_at_most_half() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let all = generations();
    publish(&mut store, &scope, &all[0]);
    store.checkpoint_write_wal().unwrap();
    let single_live = store.live_database_bytes().unwrap();
    let single_file = file_size(&directory);
    let single_blocks = store.storage_summary().unwrap();
    for facts in &all[1..] {
        let ready = publish(&mut store, &scope, facts);
        assert_eq!(
            store.current_ready(scope.workspace_id()).unwrap(),
            ready,
            "the new generation is the ready head"
        );
    }
    store.checkpoint_write_wal().unwrap();
    let twenty_live = store.live_database_bytes().unwrap();
    let twenty_file = file_size(&directory);
    let summary = store.storage_summary().unwrap();
    eprintln!(
        "single generation: live {single_live} file {single_file} blocks {}; \
         {GENERATIONS} generations: live {twenty_live} file {twenty_file} blocks {} manifest {}",
        single_blocks.blocks, summary.blocks, summary.manifest_rows
    );
    assert!(
        twenty_live * 2 <= single_live * 3,
        "live bytes {twenty_live} exceed 1.5x the single generation {single_live}"
    );
    // Unchanged files are staged as references (#2351), so the file never held
    // a second corpus next to the live graph after the first publish: it is
    // the live graph plus the few pages a checkpoint leaves free.
    assert!(
        twenty_file * 100 <= twenty_live * 105,
        "file bytes {twenty_file} exceed 1.05x the live graph {twenty_live}"
    );
    // Each later generation changes one module: at most its structure and
    // resolution blocks are new, and every other file is shared.
    let new_blocks = summary.blocks - single_blocks.blocks;
    assert!(
        new_blocks <= 2 * (GENERATIONS as u64 - 1),
        "{new_blocks} new blocks for {} changed files",
        GENERATIONS - 1
    );
    assert!(summary.manifest_rows >= single_blocks.manifest_rows * GENERATIONS as u64);
    store.verify_storage_integrity().unwrap();
}

#[test]
fn four_workspaces_at_identical_content_add_at_most_five_percent() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let facts = &generations()[0];
    let primary = store.scope(WorkspaceInstanceKey::Primary);
    publish(&mut store, &primary, facts);
    store.checkpoint_write_wal().unwrap();
    let one_live = store.live_database_bytes().unwrap();
    let one_file = file_size(&directory);
    let one = store.storage_summary().unwrap();
    let mut scopes = vec![primary];
    let mut two_file = 0;
    for index in 1..4 {
        let scope = store.scope(sandbox(index));
        publish(&mut store, &scope, facts);
        scopes.push(scope);
        if index == 1 {
            // The second workspace stages a corpus-sized payload next to the
            // live graph: the file's one-time high-water mark.
            store.checkpoint_write_wal().unwrap();
            two_file = file_size(&directory);
        }
    }
    store.checkpoint_write_wal().unwrap();
    let four_live = store.live_database_bytes().unwrap();
    let four_file = file_size(&directory);
    let four = store.storage_summary().unwrap();
    eprintln!(
        "one workspace: live {one_live} file {one_file}; four: live {four_live} file {four_file}; \
         blocks {} -> {}, manifest rows {} -> {}",
        one.blocks, four.blocks, one.manifest_rows, four.manifest_rows
    );
    assert_eq!(
        four.blocks, one.blocks,
        "identical content stores no new block"
    );
    assert_eq!(four.manifest_rows, one.manifest_rows * 4);
    assert!(
        four_live * 100 <= one_live * 105,
        "four workspaces use {four_live} bytes against {one_live} for one"
    );
    assert!(
        four_file * 100 <= two_file * 105,
        "workspaces three and four grow the file to {four_file} bytes against {two_file}"
    );
    // Every workspace reads the same facts as the primary.
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    let primary_rows = fact_rows(&connection, scopes[0].workspace_id(), 1);
    for scope in &scopes[1..] {
        assert_eq!(
            fact_rows(&connection, scope.workspace_id(), 1),
            primary_rows
        );
    }
    store.verify_storage_integrity().unwrap();
}

#[test]
fn shared_generations_read_fact_for_fact_like_a_full_copy() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let all = generations();
    for facts in all {
        publish(&mut store, &scope, facts);
    }
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    for generation in [1usize, 2, 10, GENERATIONS] {
        // A store that only ever saw this generation holds a full copy of it.
        let reference_directory = tempfile::tempdir().unwrap();
        let mut reference = open(&reference_directory);
        let reference_scope = reference.scope(WorkspaceInstanceKey::Primary);
        publish(&mut reference, &reference_scope, &all[generation - 1]);
        let reference_connection =
            Connection::open(reference_directory.path().join("graph.sqlite")).unwrap();
        let shared_rows = fact_rows(
            &connection,
            scope.workspace_id(),
            i64::try_from(generation).unwrap(),
        );
        let full_rows = fact_rows(&reference_connection, reference_scope.workspace_id(), 1);
        assert_eq!(shared_rows.len(), full_rows.len());
        for ((table, shared), (_, full)) in shared_rows.iter().zip(&full_rows) {
            assert!(!shared.is_empty() || table == "cg_unresolved_references");
            assert_eq!(shared, full, "{table} at generation {generation}");
        }
        // The query layer agrees too, on the pinned historical generation.
        let shared_query = store
            .query(
                scope.workspace_id(),
                SnapshotSelector::Generation(i64::try_from(generation).unwrap()),
            )
            .unwrap();
        let full_query = reference
            .query(
                reference_scope.workspace_id(),
                SnapshotSelector::CurrentReady,
            )
            .unwrap();
        let limits = QueryLimits {
            max_results: 128,
            max_relations: 1024,
            max_evidence_per_fact: 64,
            ..QueryLimits::default()
        };
        for index in [0usize, 1, 7, 100, 199] {
            let path = format!("src/m{index:03}.rs");
            let shared_nodes = shared_query
                .search(
                    SearchMode::ExactPath,
                    &path,
                    &QueryFilter::default(),
                    limits,
                )
                .unwrap();
            let full_nodes = full_query
                .search(
                    SearchMode::ExactPath,
                    &path,
                    &QueryFilter::default(),
                    limits,
                )
                .unwrap();
            assert_eq!(
                shared_nodes.value, full_nodes.value,
                "{path} at {generation}"
            );
            for node in &shared_nodes.value {
                let shared_explain = shared_query
                    .explain(node.id, &QueryFilter::default(), limits)
                    .unwrap();
                let full_explain = full_query
                    .explain(node.id, &QueryFilter::default(), limits)
                    .unwrap();
                assert_eq!(shared_explain.value, full_explain.value);
            }
        }
        let shared_fts = shared_query
            .search(
                SearchMode::Fts,
                "added_1",
                &QueryFilter::default(),
                QueryLimits::default(),
            )
            .unwrap();
        let full_fts = full_query
            .search(
                SearchMode::Fts,
                "added_1",
                &QueryFilter::default(),
                QueryLimits::default(),
            )
            .unwrap();
        assert_eq!(shared_fts.value, full_fts.value);
        assert_eq!(shared_fts.value.is_empty(), generation == 1);
    }
    store.verify_storage_integrity().unwrap();
}

/// Rows each shared table holds for blocks that no manifest names, and FTS rows
/// that no node row backs: both must be zero after any prune.
fn assert_no_leaks(database: &std::path::Path) {
    let connection = Connection::open(database).unwrap();
    let count = |sql: &str| -> i64 { connection.query_row(sql, [], |row| row.get(0)).unwrap() };
    assert_eq!(count("SELECT count(*) FROM cg_blocks WHERE refs <= 0"), 0);
    for table in [
        "cg_block_nodes",
        "cg_block_relations",
        "cg_block_evidence",
        "cg_block_unresolved",
    ] {
        assert_eq!(
            count(&format!(
                "SELECT count(*) FROM {table} t WHERE NOT EXISTS (SELECT 1 FROM cg_blocks b WHERE b.block_id=t.block_id)"
            )),
            0,
            "{table} rows without a block"
        );
    }
    assert_eq!(
        count("SELECT count(*) FROM cg_fts_blocks"),
        count("SELECT count(*) FROM cg_block_nodes"),
        "one FTS row per stored node"
    );
}

#[test]
fn pruning_a_generation_keeps_blocks_another_workspace_references() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let primary = store.scope(WorkspaceInstanceKey::Primary);
    let twin = store.scope(sandbox(7));
    let all = generations();
    for facts in &all[..3] {
        publish(&mut store, &primary, facts);
    }
    // A second workspace at generation-1 content keeps every generation-1 block alive.
    publish(&mut store, &twin, &all[0]);
    let before = store.storage_summary().unwrap();
    store
        .prune_detailed_generation(primary.workspace_id(), 1)
        .unwrap();
    let after = store.storage_summary().unwrap();
    assert_eq!(
        after.blocks, before.blocks,
        "the twin workspace still references every generation-1 block"
    );
    assert!(after.manifest_rows < before.manifest_rows);
    store.verify_storage_integrity().unwrap();
    assert_no_leaks(&directory.path().join("graph.sqlite"));

    // The twin reads its whole content, and the primary's head is untouched.
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    for (scope, content, generation) in [(&twin, 0usize, 1i64), (&primary, 2, 3)] {
        let reference_directory = tempfile::tempdir().unwrap();
        let mut reference = open(&reference_directory);
        let reference_scope = reference.scope(WorkspaceInstanceKey::Primary);
        publish(&mut reference, &reference_scope, &all[content]);
        let reference_connection =
            Connection::open(reference_directory.path().join("graph.sqlite")).unwrap();
        assert_eq!(
            fact_rows(&connection, scope.workspace_id(), generation),
            fact_rows(&reference_connection, reference_scope.workspace_id(), 1)
        );
    }
    assert!(
        store
            .prune_detailed_generation(primary.workspace_id(), 3)
            .is_err(),
        "the ready head is never pruned"
    );
    assert!(
        store
            .prune_detailed_generation(primary.workspace_id(), 1)
            .is_err(),
        "a pruned generation is gone"
    );
}

#[test]
fn pruning_deletes_a_blocks_rows_with_its_last_reference() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let primary = store.scope(WorkspaceInstanceKey::Primary);
    let all = generations();
    for facts in &all[..3] {
        publish(&mut store, &primary, facts);
    }
    let before = store.storage_summary().unwrap();
    // Generation 2 rewrote module 1; its generation-1 blocks die with generation 1.
    store
        .prune_detailed_generation(primary.workspace_id(), 1)
        .unwrap();
    let after = store.storage_summary().unwrap();
    let freed = before.blocks - after.blocks;
    assert!(
        (1..=2).contains(&freed),
        "module 1's replaced blocks are deleted, nothing else: {freed}"
    );
    store.verify_storage_integrity().unwrap();
    assert_no_leaks(&directory.path().join("graph.sqlite"));
    store
        .prune_detailed_generation(primary.workspace_id(), 2)
        .unwrap();
    store.verify_storage_integrity().unwrap();
    assert_no_leaks(&directory.path().join("graph.sqlite"));
    // Only the head remains, with all of its facts.
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    let reference_directory = tempfile::tempdir().unwrap();
    let mut reference = open(&reference_directory);
    let reference_scope = reference.scope(WorkspaceInstanceKey::Primary);
    publish(&mut reference, &reference_scope, &all[2]);
    let reference_connection =
        Connection::open(reference_directory.path().join("graph.sqlite")).unwrap();
    assert_eq!(
        fact_rows(&connection, primary.workspace_id(), 3),
        fact_rows(&reference_connection, reference_scope.workspace_id(), 1)
    );
    assert_eq!(
        store.storage_summary().unwrap().blocks,
        reference.storage_summary().unwrap().blocks,
        "history gone, the shared store is exactly one generation"
    );
}

#[test]
fn v8_database_converts_in_place_keeping_every_fact_and_the_sharing() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = open(&directory);
    let primary = store.scope(WorkspaceInstanceKey::Primary);
    let twin = store.scope(sandbox(9));
    let all = generations();
    for facts in &all[..3] {
        publish(&mut store, &primary, facts);
    }
    publish(&mut store, &twin, &all[1]);
    let expected = {
        let connection = Connection::open(&database).unwrap();
        let mut rows = Vec::new();
        for (workspace, generations) in [
            (primary.workspace_id(), 1..=3),
            (twin.workspace_id(), 1..=1),
        ] {
            for generation in generations {
                rows.push((
                    workspace,
                    generation,
                    fact_rows(&connection, workspace, generation),
                ));
            }
        }
        rows
    };
    let heads = [
        store.current_ready(primary.workspace_id()).unwrap(),
        store.current_ready(twin.workspace_id()).unwrap(),
    ];
    let before = store.storage_summary().unwrap();
    drop(store);

    let legacy = Connection::open(&database).unwrap();
    legacy
        .execute_batch(include_str!("fixtures/downgrade-to-v8.sql"))
        .unwrap();
    let legacy_nodes: i64 = legacy
        .query_row("SELECT count(*) FROM cg_nodes", [], |row| row.get(0))
        .unwrap();
    let table_kind: String = legacy
        .query_row(
            "SELECT type FROM sqlite_master WHERE name='cg_nodes'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        table_kind, "table",
        "the fixture restored a physical v8 table"
    );
    assert!(legacy_nodes > 0);
    drop(legacy);

    let migrated = open(&directory);
    for head in &heads {
        assert_eq!(migrated.current_ready(head.workspace_id).unwrap(), *head);
    }
    let after = migrated.storage_summary().unwrap();
    eprintln!(
        "blocks {} -> {} after conversion, manifest rows {} -> {}",
        before.blocks, after.blocks, before.manifest_rows, after.manifest_rows
    );
    assert!(
        after.blocks <= before.blocks + before.blocks / 10,
        "the conversion keeps generations sharing blocks"
    );
    migrated.verify_storage_integrity().unwrap();
    let connection = Connection::open(&database).unwrap();
    for (workspace, generation, rows) in &expected {
        assert_eq!(
            &fact_rows(&connection, *workspace, *generation),
            rows,
            "workspace {workspace} generation {generation}"
        );
    }
    let type_of = |name: &str| -> String {
        connection
            .query_row(
                "SELECT type FROM sqlite_master WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(type_of("cg_nodes"), "view");
    assert_eq!(type_of("cg_blocks"), "table");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 10);
}

#[test]
fn a_write_past_the_page_limit_is_a_disk_full_error_and_leaves_no_head() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let all = generations();
    // Admit only a sliver more than the empty database holds, as the daemon's
    // budget gate does, then stage a whole corpus into it.
    store.checkpoint_write_wal().unwrap();
    let held = file_size(&directory);
    store.constrain_database_bytes(held + 64 * 1024).unwrap();
    let owners = all[0]
        .iter()
        .map(|file| SourceVersion {
            relative_path: file.file.relative_path.clone(),
            source_digest: blake3::hash(&file.file.bytes).to_hex().to_string(),
        })
        .collect::<Vec<_>>();
    let run = store.begin_staged(&scope, &manifest(), &owners).unwrap();
    let failure = all[0]
        .iter()
        .find_map(|file| store.stage_file(&run, file).err())
        .expect("the corpus does not fit the admitted pages");
    assert!(
        failure.is_disk_full(),
        "the page limit surfaces as a typed disk-full error: {failure}"
    );
    assert!(store.current_ready(scope.workspace_id()).is_err());
    store.verify_storage_integrity().unwrap();
}

// ---- #2351: no per-workspace or per-run copy of what the blocks already hold ----

fn owners_of(facts: &[PerFileFacts]) -> Vec<SourceVersion> {
    facts
        .iter()
        .map(|file| SourceVersion {
            relative_path: file.file.relative_path.clone(),
            source_digest: blake3::hash(&file.file.bytes).to_hex().to_string(),
        })
        .collect()
}

/// Stage a whole corpus and return the run with the payload bytes it wrote.
fn stage_corpus(
    store: &mut CodegraphStore,
    directory: &tempfile::TempDir,
    scope: &WorkspaceScope,
    facts: &[PerFileFacts],
) -> (rsi_codegraph::StageRun, i64) {
    let run = store
        .begin_staged(scope, &manifest(), &owners_of(facts))
        .unwrap();
    for file in facts {
        store.stage_file(&run, file).unwrap();
    }
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    let payload = connection
        .query_row(
            "SELECT payload_bytes FROM cg_stage_runs WHERE workspace_id=?1",
            [scope.workspace_id().to_string()],
            |row| row.get(0),
        )
        .unwrap();
    (run, payload)
}

/// Publish and account a generation the way the indexer does.
fn publish_accounted(
    store: &mut CodegraphStore,
    scope: &WorkspaceScope,
    facts: &[PerFileFacts],
) -> ReadySnapshot {
    let ready = publish(store, scope, facts);
    store
        .record_published_generation(scope.workspace_id(), ready.generation)
        .unwrap();
    ready
}

fn ledger_rows(directory: &tempfile::TempDir) -> i64 {
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    ["cg_retained_nodes", "cg_retained_relations"]
        .into_iter()
        .map(|table| {
            connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        })
        .sum()
}

#[test]
fn twenty_workspaces_at_identical_content_add_at_most_five_percent_with_ledger_and_manifest() {
    const WORKSPACES: u128 = 20;
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let facts = &generations()[0];
    let primary = store.scope(WorkspaceInstanceKey::Primary);
    publish_accounted(&mut store, &primary, facts);
    store.checkpoint_write_wal().unwrap();
    let one_live = store.live_database_bytes().unwrap();
    let one_file = file_size(&directory);
    let one = store.storage_summary().unwrap();
    let manifest_of_one = store.index_manifest(primary.workspace_id()).unwrap();
    assert_eq!(manifest_of_one.len(), facts.len(), "one entry per file");
    for index in 1..WORKSPACES {
        let scope = store.scope(sandbox(index));
        publish_accounted(&mut store, &scope, facts);
        assert_eq!(
            store.index_manifest(scope.workspace_id()).unwrap(),
            manifest_of_one,
            "every workspace reads the same exact-byte manifest from the head"
        );
    }
    store.checkpoint_write_wal().unwrap();
    let live = store.live_database_bytes().unwrap();
    let file = file_size(&directory);
    let many = store.storage_summary().unwrap();
    eprintln!(
        "one workspace: live {one_live} file {one_file}; {WORKSPACES}: live {live} file {file}; \
         manifest rows {} -> {}",
        one.manifest_rows, many.manifest_rows
    );
    assert_eq!(many.blocks, one.blocks, "identical content stores no block");
    assert_eq!(ledger_rows(&directory), 0, "no identity left a generation");
    assert!(
        live * 100 <= one_live * 105,
        "{WORKSPACES} workspaces use {live} bytes against {one_live} for one"
    );
    // Workspaces after the first stage references: the file never holds a
    // second corpus next to the live graph.
    assert!(
        file * 100 <= one_file * 105,
        "the file grew to {file} bytes against {one_file} for one workspace"
    );
    store.verify_storage_integrity().unwrap();
}

#[test]
fn a_publish_of_unchanged_content_stages_under_five_percent_of_the_payload() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let all = generations();
    let primary = store.scope(WorkspaceInstanceKey::Primary);
    let (run, full) = stage_corpus(&mut store, &directory, &primary, &all[0]);
    store.publish_staged(&run, PublishFault::None).unwrap();
    assert!(full > 0);

    // The same content again, in the same workspace and in a new one.
    for scope in [primary.clone(), store.scope(sandbox(1))] {
        let (run, staged) = stage_corpus(&mut store, &directory, &scope, &all[0]);
        eprintln!("full payload {full} bytes, unchanged corpus staged {staged} bytes");
        assert!(
            staged * 100 < full * 5,
            "an unchanged corpus staged {staged} of {full} payload bytes"
        );
        let ready = store.publish_staged(&run, PublishFault::None).unwrap();
        assert_eq!(store.current_ready(scope.workspace_id()).unwrap(), ready);
    }

    // One module changed: only its facts are staged in full.
    let (run, staged) = stage_corpus(&mut store, &directory, &primary, &all[1]);
    assert!(
        staged * 100 < full * 5,
        "one changed module staged {staged} of {full} payload bytes"
    );
    store.publish_staged(&run, PublishFault::None).unwrap();

    // Reference-staged generations read fact for fact like a full copy.
    let reference_directory = tempfile::tempdir().unwrap();
    let mut reference = open(&reference_directory);
    let reference_scope = reference.scope(WorkspaceInstanceKey::Primary);
    publish(&mut reference, &reference_scope, &all[1]);
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    let reference_connection =
        Connection::open(reference_directory.path().join("graph.sqlite")).unwrap();
    assert_eq!(
        fact_rows(&connection, primary.workspace_id(), 3),
        fact_rows(&reference_connection, reference_scope.workspace_id(), 1)
    );
    store.verify_storage_integrity().unwrap();
}

#[test]
fn a_staged_reference_to_reclaimed_blocks_is_restaged_from_facts() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let all = generations();
    let primary = store.scope(WorkspaceInstanceKey::Primary);
    let twin = store.scope(sandbox(3));
    publish_accounted(&mut store, &primary, &all[0]);
    publish_accounted(&mut store, &primary, &all[1]);

    // The twin stages generation-1 content as references to blocks only the
    // primary's generation 1 still holds, then that generation is pruned.
    let (run, staged) = stage_corpus(&mut store, &directory, &twin, &all[0]);
    store
        .prune_detailed_generation(primary.workspace_id(), 1)
        .unwrap();
    assert!(matches!(
        store.publish_staged(&run, PublishFault::None),
        Err(rsi_codegraph::CodegraphError::RetryableWriterConflict)
    ));
    assert!(store.current_ready(twin.workspace_id()).is_err());
    store.verify_storage_integrity().unwrap();

    // Staging again writes the dropped facts in full and publishes them.
    let (run, restaged) = stage_corpus(&mut store, &directory, &twin, &all[0]);
    assert!(restaged > staged, "{restaged} bytes against {staged}");
    let ready = store.publish_staged(&run, PublishFault::None).unwrap();
    assert_eq!(store.current_ready(twin.workspace_id()).unwrap(), ready);
    let reference_directory = tempfile::tempdir().unwrap();
    let mut reference = open(&reference_directory);
    let reference_scope = reference.scope(WorkspaceInstanceKey::Primary);
    publish(&mut reference, &reference_scope, &all[0]);
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    let reference_connection =
        Connection::open(reference_directory.path().join("graph.sqlite")).unwrap();
    assert_eq!(
        fact_rows(&connection, twin.workspace_id(), 1),
        fact_rows(&reference_connection, reference_scope.workspace_id(), 1)
    );
    store.verify_storage_integrity().unwrap();
}

fn small_corpus(second_function: bool) -> Vec<PerFileFacts> {
    let mut source = String::from("pub fn one() {}\n");
    if second_function {
        source.push_str("pub fn two() {}\n");
    }
    ExtractionCache::new()
        .update(vec![
            SourceFile {
                relative_path: "Cargo.toml".into(),
                bytes: b"[package]\nname = \"small\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"
                    .to_vec(),
            },
            SourceFile {
                relative_path: "src/lib.rs".into(),
                bytes: source.into_bytes(),
            },
        ])
        .unwrap()
        .facts
}

#[test]
fn the_ledger_holds_only_identities_that_left_every_retained_generation() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(&directory);
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let with_two = small_corpus(true);
    let without_two = small_corpus(false);
    publish_accounted(&mut store, &scope, &with_two);
    publish_accounted(&mut store, &scope, &without_two);
    assert_eq!(
        ledger_rows(&directory),
        0,
        "the current identities and the ones a retained generation holds have no row"
    );

    // Pruning the generation that alone held `two` records its identities.
    store
        .prune_detailed_generation(scope.workspace_id(), 1)
        .unwrap();
    let connection = Connection::open(directory.path().join("graph.sqlite")).unwrap();
    let tombstoned: Vec<String> = connection
        .prepare("SELECT name FROM cg_retained_nodes WHERE workspace_id=?1 AND tombstoned=1")
        .unwrap()
        .query_map([scope.workspace_id().to_string()], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(tombstoned, vec!["two".to_owned()]);
    let live_rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM cg_retained_nodes WHERE tombstoned=0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(live_rows, 0, "no row for a node a generation still holds");
    assert!(
        ledger_rows(&directory) > 1,
        "its relations are recorded too"
    );

    // The same function comes back: it is current again, so its row goes.
    publish_accounted(&mut store, &scope, &with_two);
    assert_eq!(ledger_rows(&directory), 0);
    store.verify_storage_integrity().unwrap();
}

#[test]
fn v9_database_drops_its_manifest_copy_and_live_ledger_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut store = open(&directory);
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let facts = small_corpus(true);
    let ready = publish_accounted(&mut store, &scope, &facts);
    let expected = store.index_manifest(scope.workspace_id()).unwrap();
    assert_eq!(expected.len(), facts.len());
    drop(store);

    // Turn it into a v9 database: a manifest copy, a live ledger row per
    // identity, one tombstone of an identity the head still holds and one that
    // really left.
    let id = scope.workspace_id().to_string();
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(&format!(
            "CREATE TABLE cg_index_manifest (workspace_id TEXT NOT NULL, path TEXT NOT NULL,
               source_digest TEXT NOT NULL, PRIMARY KEY (workspace_id,path));
             INSERT INTO cg_index_manifest SELECT workspace_id,path,source_digest FROM cg_files
               WHERE workspace_id='{id}';
             INSERT INTO cg_retained_nodes SELECT workspace_id,node_id,node_key,kind,name,generation,0
               FROM cg_nodes WHERE workspace_id='{id}';
             INSERT INTO cg_retained_relations SELECT workspace_id,relation_id,relation_key,kind,
               source_id,target_id,generation,0 FROM cg_relations WHERE workspace_id='{id}';
             UPDATE cg_retained_nodes SET tombstoned=1 WHERE name='one';
             INSERT INTO cg_retained_nodes VALUES ('{id}','gone','k','Function','gone',1,1);
             INSERT INTO cg_payload_refs VALUES ('x','y','z',1,NULL);
             PRAGMA user_version = 9;"
        ))
        .unwrap();
    drop(legacy);

    let store = open(&directory);
    let connection = Connection::open(&path).unwrap();
    let count = |sql: &str| -> i64 { connection.query_row(sql, [], |row| row.get(0)).unwrap() };
    assert_eq!(
        count("SELECT count(*) FROM sqlite_master WHERE name='cg_index_manifest'"),
        0
    );
    assert_eq!(count("SELECT count(*) FROM cg_payload_refs"), 0);
    assert_eq!(count("SELECT count(*) FROM cg_retained_relations"), 0);
    let kept: Vec<String> = connection
        .prepare("SELECT node_id FROM cg_retained_nodes")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        kept,
        vec!["gone".to_owned()],
        "only the real tombstone stays"
    );
    assert_eq!(
        store.index_manifest(scope.workspace_id()).unwrap(),
        expected
    );
    assert_eq!(store.current_ready(scope.workspace_id()).unwrap(), ready);
}
