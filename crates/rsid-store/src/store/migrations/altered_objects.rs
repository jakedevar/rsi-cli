// The one place that says which pre-existing catalog objects a migration alters.
//
// A migration that `ALTER`s, drops or rebuilds an object an older migration
// created changes that object's `sqlite_master` SQL, so a test that pins the
// pre-existing catalog across a rewind-and-replay must not demand the old text
// for it. Tests call `assert_catalog_preserved_across_replay` (below), which
// reads this table: an object listed for a version above the rewind point must
// end the replay either unchanged or byte-equal to the head catalog; every other
// pre-existing object must be unchanged exactly. No test keeps its own list.
//
// `tools/check-released-migrations.py` refuses a NEW migration file whose SQL
// `ALTER`s, drops or renames an existing object, or calls a `rebuild_*` helper,
// without an entry here, and names the entry to add. It is deliberately
// conservative: a false refusal costs one line here, a missed entry costs a
// merge-queue gate round. It reads only the migration file, so a rebuild hidden
// behind a helper not named `rebuild_*` is not seen: list it yourself.
//
// Entry format (the guard parses it): `(version, &["object", ...]),`.

/// The table is complete for every version ABOVE this one. A test that rewinds
/// below it panics until the missing versions are backfilled (read each
/// `migrations/vNNN.rs` for `ALTER`, `DROP`, rename or rebuild of older objects).
#[cfg(test)]
pub(crate) const MIGRATION_ALTERED_OBJECTS_COMPLETE_ABOVE: i32 = 160;

/// Pre-existing objects each migration alters, by migration version.
#[cfg(test)]
#[rustfmt::skip]
pub(crate) const MIGRATION_ALTERED_OBJECTS: &[(i32, &[&str])] = &[
    (185, &["rolling_queue_entries"]),
    // #1909: the Deleted-owner status constraint rebuilds the items table.
    (168, &["source_worktree_settlement_items"]),
    // #2012: the launch record gains its worker scope.
    (169, &["provider_turn_custody"]),
    // #2021: the appointment row gains its seat-handoff reference.
    (170, &["manager_portfolio_appointments"]),
    // RSI-RELEASED-MIGRATION-BEGIN: v171-settlement-altered-objects
    // #2098: one audited exception for pre-removal proof fences.
    (171, &["source_worktree_settlement_items_forward_phase", "source_worktree_settlement_items_terminal"]),
    // RSI-RELEASED-MIGRATION-END: v171-settlement-altered-objects
    // RSI-RELEASED-MIGRATION-BEGIN: v172-settlement-completion-altered-objects
    (172, &["source_worktree_settlement_items_forward_phase", "source_worktree_settlement_items_terminal", "source_worktree_settlement_current_runs"]),
    // RSI-RELEASED-MIGRATION-END: v172-settlement-completion-altered-objects
    // RSI-RELEASED-MIGRATION-BEGIN: v173-settlement-untouched-altered-objects
    (173, &["settlement_completion_retries_insert"]),
    // RSI-RELEASED-MIGRATION-END: v173-settlement-untouched-altered-objects
    // RSI-RELEASED-MIGRATION-BEGIN: v175-session-health-altered-objects
    // #2050: the dependency-health evidence view moves to the V175 relevance classifier.
    (175, &["source_worktree_session_dependency_evidence_v120"]),
    // RSI-RELEASED-MIGRATION-END: v175-session-health-altered-objects
    // #2384: provenance stays retained except inside the cold-archive move transaction.
    (176, &["conversation_event_provenance_no_delete"]),
    // RSI-RELEASED-MIGRATION-BEGIN: v180-deploy-request-identity-altered-objects
    // #1572: the deploy table is rebuilt with an immutable request identity.
    (
        180,
        &[
            "agent_deploys",
            "agent_deploys_one_live",
            "agent_deploys_no_delete",
            "agent_deploys_terminal_immutable",
        ],
    ),
    // RSI-RELEASED-MIGRATION-END: v180-deploy-request-identity-altered-objects
    (182, &["topologies", "sqlite_autoindex_topologies_2"]),
    // #3181: the jobs table is rebuilt to admit the `bench` kind.
    (187, &["agent_jobs", "agent_jobs_owner_key", "agent_jobs_by_state", "agent_jobs_by_owner", "agent_jobs_no_delete", "agent_jobs_terminal_immutable", "agent_jobs_rebuild_old"]),
];

/// Names of the objects altered by migrations above `version`.
#[cfg(test)]
pub(crate) fn migration_altered_objects_above(
    version: i32,
) -> std::collections::BTreeSet<&'static str> {
    assert!(
        version >= MIGRATION_ALTERED_OBJECTS_COMPLETE_ABOVE,
        "MIGRATION_ALTERED_OBJECTS is complete only above V{MIGRATION_ALTERED_OBJECTS_COMPLETE_ABOVE}; \
         backfill the versions above V{version} in migrations/altered_objects.rs and lower \
         MIGRATION_ALTERED_OBJECTS_COMPLETE_ABOVE"
    );
    MIGRATION_ALTERED_OBJECTS
        .iter()
        .filter(|(altered_in, _)| *altered_in > version)
        .flat_map(|(_, objects)| objects.iter().copied())
        .collect()
}

/// `(name, sql)` of every `sqlite_master` row, by name.
#[cfg(test)]
pub(crate) fn catalog_sql_by_name(connection: &Connection) -> Vec<(String, String)> {
    connection
        .prepare("SELECT name,COALESCE(sql,'') FROM sqlite_master ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// After a store rewound to `rewound_to` (`old_catalog` taken there) is replayed
/// to the head: every pre-existing object keeps its exact SQL, except an object
/// a later migration alters (the table above), which must equal the head catalog.
#[cfg(test)]
pub(crate) fn assert_catalog_preserved_across_replay(
    old_catalog: &[(String, String)],
    replayed: &Connection,
    rewound_to: i32,
) {
    let altered = migration_altered_objects_above(rewound_to);
    let head = Store::open_in_memory().unwrap();
    let head_catalog = catalog_sql_by_name(&head.conn);
    let replayed_catalog = catalog_sql_by_name(replayed);
    for (name, old_sql) in old_catalog {
        let actual = replayed_catalog
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, sql)| sql);
        // A replaced constraint can remove an implicit index altogether.
        if actual.is_none() && altered.contains(name.as_str()) {
            assert!(
                head_catalog.iter().all(|(candidate, _)| candidate != name),
                "replay dropped an object the head retains: {name}"
            );
            continue;
        }
        let actual = actual.unwrap_or_else(|| panic!("replay dropped catalog object {name}"));
        if altered.contains(name.as_str()) {
            let head_sql = head_catalog
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, sql)| sql);
            assert!(
                actual == old_sql || Some(actual) == head_sql,
                "altered object {name} is neither unchanged nor the head catalog after replay"
            );
        } else {
            assert_eq!(
                actual, old_sql,
                "catalog object {name} changed across replay but no migration above \
                 V{rewound_to} lists it in MIGRATION_ALTERED_OBJECTS"
            );
        }
    }
}
