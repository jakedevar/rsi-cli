use super::{
    filters::BROAD_TEST_LIMIT,
    git::Hunk,
    identity::ItemReferences,
    relocation::{private_move, relocation_lines},
    seeds::changed_seed_proof,
    source::SourceTree,
    trace::{self, Bounds, Stop},
};
use std::collections::{BTreeMap, BTreeSet};

const PATH: &str = "crates/rsid/src/session/mod.rs";
const BEFORE: &str = "use std::collections::HashMap;\nuse uuid::Uuid;\nmod tests;\npub(crate) struct Registry {\n    by_session: HashMap<Uuid, String>,\n}\nimpl Registry {\n    pub(crate) fn insert(&mut self, token: String, id: Uuid) {\n        self.by_session.insert(id, token);\n    }\n}\nfn remint_token(registry: &mut Registry) {\n    registry.insert(token, id);\n}\n";
const AFTER: &str = "use std::collections::HashMap;\nuse uuid::Uuid;\nmod tests;\nmod registry {\n    use std::collections::HashMap;\n    use uuid::Uuid;\n    #[derive(Debug, Clone, Copy)]\n    pub(crate) struct TransportWitness {\n        incarnation_id: Uuid,\n    }\n    #[derive(Debug)]\n    struct Binding {\n        token: String,\n        witness: TransportWitness,\n    }\n    pub(crate) struct Registry {\n        by_session: HashMap<Uuid, Binding>,\n    }\n    impl Registry {\n        #[cfg(test)]\n        pub(crate) fn insert(&mut self, token: String, id: Uuid) {\n            self.insert_binding(token, id);\n        }\n        fn insert_binding(&mut self, token: String, id: Uuid) {\n            let binding = Binding { token, witness };\n            self.by_session.insert(id, binding);\n        }\n        pub(super) fn remint(&mut self, token: String, id: Uuid) {\n            self.insert_binding(token, id);\n        }\n        pub(crate) fn binding_for_session(&self, id: Uuid) -> TransportWitness {\n            self.by_session.get(id).witness\n        }\n    }\n}\npub(crate) use registry::Registry;\nfn remint_token(registry: &mut Registry) {\n    registry.remint(token, id);\n}\n";

fn gate(name: &str, body: &str) -> String {
    format!(
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-session-03\"))]\n#[test]\nfn {name}() {{\n    {body}\n}}\n"
    )
}
fn fixture(text: &str, helper: bool, observer: bool) -> SourceTree {
    let mut tests: String = [
        (
            "registry_bijective",
            if helper {
                "seed_registry();"
            } else {
                "Registry::default().insert(token, id);"
            },
        ),
        ("registry_remint", "Registry::default().insert(token, id);"),
        ("registry_witness", "registry.binding_for_session(id);"),
        ("registry_cleanup", "registry.binding_for_session(id);"),
        ("continue_registers_token", "remint_token(registry);"),
        ("remint_revokes_token", "remint_token(registry);"),
    ]
    .into_iter()
    .map(|(n, b)| gate(n, b))
    .collect();
    tests.push_str(
        &(0..70)
            .map(|n| gate(&format!("unrelated_{n}"), "unrelated_insert();"))
            .collect::<String>(),
    );
    tests.push_str("fn unrelated_insert() {\n    map.insert(token, witness);\n}\n");
    if helper {
        tests.push_str("fn seed_registry() {\n    Registry::default().insert(token, id);\n}\n");
    }
    let mut files = BTreeMap::from([
        ("crates/rsid/src/lib.rs".into(), "pub mod session;\n".into()),
        (PATH.into(), text.into()),
        ("crates/rsid/src/session/tests.rs".into(), tests),
    ]);
    if observer {
        files.insert("crates/rsid/src/session/observer.rs".into(), format!("struct RegistryObserver {{\n    registry: Registry,\n}}\nimpl RegistryObserver {{\n    fn reconcile(&self) {{\n        self.registry.binding_for_session(id);\n    }}\n}}\n{}", gate("registry_observer", "RegistryObserver::new().reconcile();")));
        files.insert("crates/rsid/src/session/unrelated.rs".into(), format!("struct OtherObserver {{\n    value: String,\n}}\nimpl OtherObserver {{\n    fn reconcile(&self) {{\n        other_work();\n    }}\n}}\n{}", gate("other_observer", "OtherObserver::new().reconcile();")));
    }
    SourceTree::new(files, "linux")
}
fn hunks(after: &str) -> Vec<Hunk> {
    vec![Hunk {
        old_first: 1,
        old_count: BEFORE.lines().count(),
        new_first: 1,
        new_count: after.lines().count(),
    }]
}
fn selected(after: &str, helper: bool, observer: bool) -> Vec<String> {
    let sources = fixture(after, helper, observer);
    let old = fixture(BEFORE, helper, observer);
    let mut index = ItemReferences::new(&sources).unwrap();
    let proof = changed_seed_proof(&sources, &old, &index, PATH, &hunks(after)).unwrap();
    assert_eq!(
        proof.moved,
        BTreeSet::from([
            "Registry".into(),
            "Binding".into(),
            "TransportWitness".into()
        ])
    );
    let trace = trace::trace_proof(
        &mut index,
        &proof,
        Bounds {
            tests: BROAD_TEST_LIMIT,
            ..Bounds::default()
        },
    )
    .unwrap();
    assert_eq!(trace.stop, None);
    trace.tests.unwrap().into_iter().map(|t| t.name).collect()
}
fn expected(observer: bool) -> Vec<String> {
    let mut names = vec![
        "continue_registers_token",
        "registry_bijective",
        "registry_cleanup",
        "registry_remint",
        "registry_witness",
        "remint_revokes_token",
    ];
    if observer {
        names.insert(0, "registry_observer");
    }
    names.into_iter().map(str::to_owned).collect()
}

#[test]
fn private_registry_move_reaches_six_real_users_through_concrete_test_methods() {
    assert_eq!(selected(AFTER, false, false), expected(false));
}

#[test]
fn private_test_method_consumers_trace_through_a_fixture_helper() {
    assert_eq!(selected(AFTER, true, false), expected(false));
}

#[test]
fn private_member_readers_trace_their_receiver_across_modules() {
    assert_eq!(selected(AFTER, false, true), expected(true));
}

#[test]
fn unrelated_declarations_and_unresolved_private_imports_keep_broad_coverage() {
    for after in [
        format!("{AFTER}pub const LIMIT: u32 = 2;\n"),
        AFTER.replacen("    use uuid::Uuid;", "    use another::Uuid;", 1),
        AFTER.replacen("    use uuid::Uuid;", "    use uuid::Uuid as Other;", 1),
        AFTER.replacen("    use uuid::Uuid;", "    use uuid::*;", 1),
    ] {
        let sources = fixture(&after, false, false);
        let old = fixture(BEFORE, false, false);
        let index = ItemReferences::new(&sources).unwrap();
        assert!(changed_seed_proof(&sources, &old, &index, PATH, &hunks(&after)).is_err());
    }
}

#[test]
fn moved_struct_alignment_ignores_only_the_proven_item_indentation() {
    let before =
        "#[derive(Debug)]\nstruct State {\n    value: u32,\n}\nconst UNRELATED: u32 = 1;\n";
    let after = "mod inner {\n    #[derive(Debug)]\n    struct State {\n        value: u64,\n    }\n}\n const UNRELATED: u32 = 1;\n";
    let relocation = private_move(before, after).unwrap();
    assert_eq!(
        relocation.moved,
        BTreeMap::from([("State".into(), "inner".into())])
    );
    assert_eq!(relocation.regions, [(1, 6)]);
    let (old, new) = relocation_lines(before, after, &relocation, 1000).unwrap();
    assert_eq!(old, BTreeSet::from([3, 5]));
    assert_eq!(new, BTreeSet::from([1, 4, 6, 7]));
}

#[test]
fn alignment_preserves_repeated_lines_and_earliest_old_new_match_ties() {
    let before = "a\nb\na\nb\n";
    let after = "b\na\nb\na\n";
    let relocation = private_move(before, after).unwrap();
    let (old, new) = relocation_lines(before, after, &relocation, 100).unwrap();
    assert_eq!(old, BTreeSet::from([4]));
    assert_eq!(new, BTreeSet::from([1]));
    assert_eq!(
        relocation_lines(before, before, &relocation, 100).unwrap(),
        (BTreeSet::new(), BTreeSet::new())
    );
}

#[test]
fn alignment_preserves_semantic_whitespace_inside_multiline_literals() {
    let before = "struct State {\n    value: u32,\n}\nimpl State {\n    fn text() {\n        let text = r#\"first\n    inside\nlast\"#;\n    }\n}\n";
    let after = "mod inner {\n    struct State {\n        value: u32,\n    }\n    impl State {\n        fn text() {\n            let text = r#\"first\n        inside\nlast\"#;\n        }\n    }\n}\n";
    let relocation = private_move(before, after).unwrap();
    let (old, new) = relocation_lines(before, after, &relocation, 1000).unwrap();
    assert!(old.contains(&7));
    assert!(new.contains(&8));
}

#[test]
fn alignment_exhaustion_refuses_instead_of_returning_a_partial_diff() {
    let relocation = private_move(BEFORE, AFTER).unwrap();
    assert_eq!(
        relocation_lines(BEFORE, AFTER, &relocation, 0).unwrap_err(),
        "private relocation alignment exceeds its comparison bound"
    );
}

#[test]
fn only_closed_private_modules_and_real_structs_prove_relocation() {
    let before = "struct State {\n    value: u32,\n}\n";
    for after in [
        "pub mod inner {\n    struct State {\n        value: u32,\n    }\n}\n",
        "mod inner {\n    struct State {\n        value: u32,\n    }\n",
        "const FIXTURE: &str = r#\"mod inner {\n    struct State {\n        value: u32,\n    }\n}\"#;\n",
    ] {
        assert_eq!(private_move(before, after).unwrap().moved, BTreeMap::new());
    }
}

#[test]
fn duplicate_relocated_type_declarations_keep_the_proof_unresolved() {
    let mut sources = fixture(AFTER, false, false);
    sources.texts.insert(
        "crates/rsid/src/other.rs".into(),
        "struct Registry {\n    value: u32,\n}\n".into(),
    );
    let index = ItemReferences::new(&sources).unwrap();
    assert_eq!(
        changed_seed_proof(
            &sources,
            &fixture(BEFORE, false, false),
            &index,
            PATH,
            &hunks(AFTER)
        )
        .unwrap_err(),
        "relocated type has no unique concrete declaration"
    );
}

#[test]
fn ambiguous_relocated_test_method_owners_refuse_the_trace() {
    let sources = fixture(AFTER, false, false);
    let mut index = ItemReferences::new(&sources).unwrap();
    let proof = changed_seed_proof(
        &sources,
        &fixture(BEFORE, false, false),
        &index,
        PATH,
        &hunks(AFTER),
    )
    .unwrap();
    let mut fake = index
        .functions
        .iter()
        .find(|f| f.function.name == "insert")
        .unwrap()
        .clone();
    fake.owner.as_mut().unwrap().name = Some("Binding".into());
    index.functions.push(fake);
    assert_eq!(
        trace::trace_proof(&mut index, &proof, Bounds::default()).unwrap_err(),
        "relocated test method has ambiguous concrete receivers"
    );
}

#[test]
fn relocation_trace_retains_broad_coverage_and_direct_priorities_at_a_small_bound() {
    let sources = fixture(AFTER, false, false);
    let mut index = ItemReferences::new(&sources).unwrap();
    let proof = changed_seed_proof(
        &sources,
        &fixture(BEFORE, false, false),
        &index,
        PATH,
        &hunks(AFTER),
    )
    .unwrap();
    let trace = trace::trace_proof(
        &mut index,
        &proof,
        Bounds {
            tests: 1,
            ..Bounds::default()
        },
    )
    .unwrap();
    assert_eq!(trace.stop, Some(Stop::TestBound));
    assert_eq!(trace.tests, None);
    assert_eq!(trace.direct.len(), 6);
}
