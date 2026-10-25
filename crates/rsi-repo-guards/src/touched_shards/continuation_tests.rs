//! Further intent ports. Expectations are independent of legacy execution.
use super::{git::*, identity::*, selection::*, source::*, trace::*};
use std::collections::{BTreeMap, BTreeSet};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).into()).collect()
}
fn tree(items: &[(&str, &str)]) -> SourceTree {
    SourceTree::new(
        items
            .iter()
            .map(|(p, t)| ((*p).into(), (*t).into()))
            .collect(),
        "linux",
    )
}
fn index(items: &[(&str, &str)]) -> ItemReferences {
    ItemReferences::new(&tree(items)).unwrap()
}
fn items(names: &[(&str, &str)]) -> BTreeSet<Item> {
    names
        .iter()
        .map(|(m, n)| ((*m).into(), (*n).into()))
        .collect()
}
fn gate(shard: &str, name: &str, body: &str) -> String {
    format!(
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-{shard}\"))]\n#[test]\nfn {name}() {{\n    {body}\n}}\n"
    )
}
fn additions(first: usize, count: usize) -> Vec<Hunk> {
    vec![Hunk {
        old_first: first.saturating_sub(1),
        old_count: 0,
        new_first: first,
        new_count: count,
    }]
}

#[test]
fn registration_addition_removal_and_aliases_expand_both_pinned_trees() {
    let path = "crates/rsid/src/session/mod.rs";
    let old = tree(&[
        (path, "mod old;\nmod new;\npub fn marker() {}\n"),
        ("crates/rsid/src/session/old.rs", ""),
        ("crates/rsid/src/session/new.rs", ""),
    ]);
    for registration in [
        "mod new;",
        "pub use new;",
        "pub(crate) use new as renamed;",
        "pub use crate::session::new;",
        "pub use super::session::new::Item;",
        "pub use self::new::{Item, Other};",
        "pub use self::new::*;",
    ] {
        let after = tree(&[
            (path, &format!("{registration}\npub fn marker() {{}}\n")),
            ("crates/rsid/src/session/old.rs", ""),
            ("crates/rsid/src/session/new.rs", ""),
        ]);
        let hunks = vec![Hunk {
            old_first: 1,
            old_count: 2,
            new_first: 1,
            new_count: 1,
        }];
        assert_eq!(
            registration_sources(path, &after, &old, &hunks)
                .into_iter()
                .collect::<BTreeSet<_>>(),
            set(&[
                "crates/rsid/src/session/old.rs",
                "crates/rsid/src/session/new.rs"
            ]),
            "{registration}"
        );
    }
}

#[test]
fn registration_path_mounts_use_module_identity_and_current_shard_ownership() {
    let path = "crates/rsid-store/src/store/pending.rs";
    let test = gate("store-04", "new_answer", "");
    let old = tree(&[
        ("crates/rsid-store/src/lib.rs", "pub mod store;\n"),
        ("crates/rsid-store/src/store/mod.rs", "pub mod pending;\n"),
        (path, "pub fn old() {}\n"),
    ]);
    let after = tree(&[
        ("crates/rsid-store/src/lib.rs", "pub mod store;\n"),
        ("crates/rsid-store/src/store/mod.rs", "pub mod pending;\n"),
        (
            path,
            "#[path = \"remote.rs\"]\npub mod remote;\npub fn old() {}\n",
        ),
        (
            "crates/rsid-store/src/store/remote.rs",
            "#[cfg(test)]\nmod tests;\n",
        ),
        ("crates/rsid-store/src/store/tests.rs", &test),
        (
            "crates/rsid-store/src/store/remote/tests.rs",
            &gate("store-02", "stray", ""),
        ),
    ]);
    let targets = registration_sources(path, &after, &old, &additions(1, 2));
    assert_eq!(targets, ["crates/rsid-store/src/store/remote.rs"]);
    assert_eq!(after.shards_for(&targets[0]).unwrap(), set(&["store-04"]));
}

#[test]
fn uncertain_registration_or_executable_edits_retain_source_coverage() {
    let path = "crates/rsid/src/session/mod.rs";
    let old = tree(&[
        (path, "pub fn marker() {}\n"),
        ("crates/rsid/src/session/new.rs", ""),
    ]);
    for declaration in [
        "pub fn marker() { work(); }",
        "#[cfg(test)]",
        "mod inline {",
        "pub use missing::Item;",
        "#[path = \"new.rs\"]\nfn marker() {}",
    ] {
        let after = tree(&[(path, declaration), ("crates/rsid/src/session/new.rs", "")]);
        let hunks = vec![Hunk {
            old_first: 1,
            old_count: 1,
            new_first: 1,
            new_count: declaration.lines().count(),
        }];
        assert_eq!(
            registration_sources(path, &after, &old, &hunks),
            [path],
            "{declaration}"
        );
    }
}

#[test]
fn changed_tests_keep_attribute_body_and_multishard_priorities() {
    let path = "crates/rsid/src/session/tests.rs";
    let text = format!(
        "{}\n{}",
        gate("other-01", "first", "work();"),
        gate("other-02", "second", "work();")
    );
    let source = tree(&[(path, &text)]);
    let funcs = rust_functions(&text, "linux");
    for f in funcs {
        for line in [f.attr_start + 1, f.start + 1, f.end - 1] {
            let picked = changed_test_observers(&source, path, &additions(line, 1));
            assert_eq!(picked.len(), 1);
            assert_eq!(picked[0].name, f.name);
        }
    }
    assert!(
        changed_test_observers(
            &source,
            path,
            &[Hunk {
                old_first: 1,
                old_count: 2,
                new_first: 0,
                new_count: 0
            }]
        )
        .is_empty()
    );
    let text = "#[cfg(any(feature = \"test-shard-store-01\", feature = \"test-shard-other-02\"))]\n#[test]\nfn shared() {}\n";
    let source = tree(&[(path, text)]);
    assert_eq!(
        changed_test_observers(&source, path, &additions(1, 1))
            .iter()
            .map(|o| o.shard.clone())
            .collect::<BTreeSet<_>>(),
        set(&["store-01", "other-02"])
    );
}

#[test]
fn changed_test_priorities_follow_host_gates_and_skip_production() {
    let path = "crates/rsid/src/session/tests.rs";
    let text = format!(
        "#[cfg(target_os = \"macos\")]\n{}\npub fn production() {{}}\n",
        gate("other-01", "mac", "")
    );
    let sources = tree(&[(path, &text)]);
    assert_eq!(
        changed_test_observers(&sources, path, &additions(1, text.lines().count())),
        Vec::new()
    );
}

#[test]
fn use_trees_expand_self_nested_globs_and_renames_without_losing_paths() {
    let uses =
        expand_use("crate::real::{self, tools::{execute as run, nested::*}, other}").unwrap();
    assert_eq!(
        uses,
        vec![
            Import {
                path: vec!["crate".into(), "real".into()],
                bound: Some("real".into()),
                glob: false
            },
            Import {
                path: vec![
                    "crate".into(),
                    "real".into(),
                    "tools".into(),
                    "execute".into()
                ],
                bound: Some("run".into()),
                glob: false
            },
            Import {
                path: vec![
                    "crate".into(),
                    "real".into(),
                    "tools".into(),
                    "nested".into()
                ],
                bound: None,
                glob: true
            },
            Import {
                path: vec!["crate".into(), "real".into(), "other".into()],
                bound: Some("other".into()),
                glob: false
            }
        ]
    );
    assert!(expand_use("crate::{unclosed").is_err());
}

#[test]
fn qualified_item_callers_distinguish_same_name_private_items() {
    let mut refs = index(&[
        ("crates/rsid/src/tools.rs", "pub fn execute_tool() {}\n"),
        (
            "crates/rsid/src/fake.rs",
            "fn execute_tool() {}\nfn fake_user() {\n    execute_tool();\n}\n",
        ),
        (
            "crates/rsid/src/session.rs",
            "fn real_user() {\n    crate::tools::execute_tool();\n}\n",
        ),
    ]);
    let found = refs.callers("tools", "execute_tool");
    assert_eq!(found.unresolved, None);
    assert_eq!(found.functions, items(&[("session", "real_user")]));
    assert_eq!(
        refs.callers("tools", "execute_tool").functions,
        found.functions
    );
}

#[test]
fn explicit_glob_inline_and_alias_imports_reach_only_their_defining_item() {
    let mut refs = index(&[
        ("crates/rsid/src/tools.rs", "pub fn execute_tool() {}\n"),
        ("crates/rsid/src/fake.rs", "pub fn execute_tool() {}\n"),
        (
            "crates/rsid/src/session.rs",
            "use crate::tools::execute_tool as run;\nfn caller() {\n    run();\n}\nmod tests {\n    use crate::tools::*;\n    fn inline_caller() {\n        execute_tool();\n    }\n}\n",
        ),
        (
            "crates/rsid/src/fake_user.rs",
            "use crate::fake::execute_tool as run;\nfn fake() {\n    run();\n}\n",
        ),
    ]);
    let found = refs.callers("tools", "execute_tool");
    assert_eq!(found.unresolved, None);
    assert_eq!(
        found.functions,
        items(&[("session", "caller"), ("session", "inline_caller")])
    );
}

#[test]
fn pub_use_chains_and_reexported_module_aliases_resolve_to_original_items() {
    let mut refs = index(&[
        (
            "crates/rsid/src/real.rs",
            "pub mod tools {\n    pub fn execute_tool() {}\n}\n",
        ),
        (
            "crates/rsid/src/facade.rs",
            "pub use crate::real::tools as public_tools;\n",
        ),
        (
            "crates/rsid/src/bridge.rs",
            "pub use crate::facade::public_tools::*;\n",
        ),
        (
            "crates/rsid/src/session.rs",
            "use crate::bridge::execute_tool as run;\nfn caller() {\n    run();\n}\nfn qualified() {\n    crate::facade::public_tools::execute_tool();\n}\n",
        ),
    ]);
    let found = refs.callers("real", "execute_tool");
    assert_eq!(found.unresolved, None);
    assert_eq!(
        found.functions,
        items(&[("session", "caller"), ("session", "qualified")])
    );
}

#[test]
fn module_segments_and_local_bindings_do_not_create_function_edges() {
    let mut refs = index(&[
        ("crates/rsid/src/tools.rs", "pub fn execute_tool() {}\n"),
        ("crates/rsid/src/execute_tool.rs", "pub fn other() {}\n"),
        (
            "crates/rsid/src/session.rs",
            "use crate::tools::*;\nfn params(execute_tool: fn()) {\n    execute_tool();\n}\nfn shadowed() {\n    let execute_tool = || ();\n    execute_tool();\n}\nfn module_user() {\n    crate::execute_tool::other();\n}\nfn actual() {\n    execute_tool();\n}\n",
        ),
    ]);
    let found = refs.callers("tools", "execute_tool");
    assert_eq!(found.unresolved, None);
    assert_eq!(found.functions, items(&[("session", "actual")]));
}

#[test]
fn reexport_cycle_ambiguity_depth_and_unknown_local_imports_keep_broad_diagnostics() {
    for (a, b, expected) in [
        (
            "pub use crate::b::execute_tool;",
            "pub use crate::a::execute_tool;",
            "cycle",
        ),
        (
            "pub use crate::tools::*;\npub use crate::other::*;",
            "",
            "different items",
        ),
        (
            "pub use crate::missing::execute_tool;",
            "",
            "no known module",
        ),
    ] {
        let mut refs = index(&[
            ("crates/rsid/src/tools.rs", "pub fn execute_tool() {}\n"),
            ("crates/rsid/src/other.rs", "pub fn execute_tool() {}\n"),
            ("crates/rsid/src/a.rs", a),
            ("crates/rsid/src/b.rs", b),
            (
                "crates/rsid/src/session.rs",
                "fn caller() {\n    crate::a::execute_tool();\n}\n",
            ),
        ]);
        assert!(
            refs.callers("tools", "execute_tool")
                .unresolved
                .unwrap()
                .contains(expected)
        );
    }
    let mut texts = BTreeMap::from([
        (
            "crates/rsid/src/tools.rs".into(),
            "pub fn execute_tool() {}\n".into(),
        ),
        (
            "crates/rsid/src/session.rs".into(),
            "fn caller() {\n    crate::a0::execute_tool();\n}\n".into(),
        ),
    ]);
    for n in 0..8 {
        texts.insert(
            format!("crates/rsid/src/a{n}.rs"),
            if n == 7 {
                "pub use crate::tools::execute_tool;\n".into()
            } else {
                format!("pub use crate::a{}::execute_tool;\n", n + 1)
            },
        );
    }
    let mut refs = ItemReferences::new(&SourceTree::new(texts, "linux")).unwrap();
    assert!(
        refs.callers("tools", "execute_tool")
            .unresolved
            .unwrap()
            .contains("deeper")
    );
}

#[test]
fn inherent_type_alias_and_self_calls_resolve_but_unknown_receivers_remain_broad() {
    let owner = "pub struct Registry;\nimpl Registry {\n    pub fn execute() {}\n    pub fn invoke() {\n        Self::execute();\n    }\n}\n";
    let mut refs = index(&[
        ("crates/rsid/src/tools.rs", owner),
        (
            "crates/rsid/src/session.rs",
            "type Alias = crate::tools::Registry;\nfn typed() {\n    Alias::execute();\n}\n",
        ),
    ]);
    let found = refs.callers("tools", "execute");
    assert_eq!(found.unresolved, None);
    assert_eq!(
        found.functions,
        items(&[("tools", "invoke"), ("session", "typed")])
    );
    let mut refs = index(&[
        (
            "crates/rsid/src/tools.rs",
            "impl Registry {\n    pub fn execute(&self) {}\n}\n",
        ),
        (
            "crates/rsid/src/session.rs",
            "fn unknown() {\n    receiver.execute();\n}\n",
        ),
    ]);
    assert!(
        refs.callers("tools", "execute")
            .unresolved
            .unwrap()
            .contains("unknown receiver")
    );
}

#[test]
fn type_inventory_and_call_edges_ignore_quoted_fixtures_and_comments() {
    let mut refs = index(&[
        (
            "crates/rsid/src/tools.rs",
            "pub struct Registry;\npub fn execute() {}\nconst FIXTURE: &str = \"struct Registry {}\";\n",
        ),
        (
            "crates/rsid/src/session.rs",
            "fn quoted() {\n    let text = \"crate::tools::execute()\";\n    // crate::tools::execute();\n}\n",
        ),
    ]);
    assert_eq!(refs.type_counts["Registry"], 1);
    assert_eq!(refs.callers("tools", "execute").functions, BTreeSet::new());
}

#[test]
fn seeded_trace_settles_nearest_direct_and_transitive_observers() {
    let text = format!(
        "fn changed() {{}}\nfn helper() {{\n    changed();\n}}\n{}{}",
        gate("other-01", "near", "changed();"),
        gate("other-02", "far", "helper();")
    );
    let mut refs = index(&[("crates/rsid/src/session/tests.rs", &text)]);
    let result = trace_tokens(
        &mut refs,
        &Seeds {
            changed: set(&["changed"]),
            ..Seeds::default()
        },
        Bounds::default(),
    );
    assert_eq!(result.stop, None);
    assert_eq!(result.tests.as_ref().unwrap().len(), 2);
    assert_eq!(
        result
            .direct
            .iter()
            .map(|(o, d)| (o.name.clone(), *d))
            .collect::<BTreeMap<_, _>>(),
        BTreeMap::from([("near".into(), 1), ("far".into(), 2)])
    );
}

#[test]
fn bounded_trace_keeps_settled_layers_and_all_test_identities_as_priorities() {
    let text = format!(
        "fn changed() {{}}\nfn helper() {{\n    changed();\n}}\nfn far_helper() {{\n    helper();\n}}\n{}{}{}",
        gate("other-01", "near", "changed();"),
        gate("other-02", "middle", "helper();"),
        gate("other-02", "far", "far_helper();")
    );
    let mut refs = index(&[("crates/rsid/src/session/tests.rs", &text)]);
    let seeds = Seeds {
        changed: set(&["changed"]),
        ..Seeds::default()
    };
    let result = trace_tokens(
        &mut refs,
        &seeds,
        Bounds {
            functions: 2,
            ..Bounds::default()
        },
    );
    assert_eq!(result.stop, Some(Stop::FunctionBound));
    assert_eq!(result.tests, None);
    assert_eq!(
        result
            .direct
            .keys()
            .map(|o| o.name.clone())
            .collect::<BTreeSet<_>>(),
        set(&["near", "middle"])
    );
    let result = trace_tokens(
        &mut refs,
        &seeds,
        Bounds {
            tests: 1,
            ..Bounds::default()
        },
    );
    assert_eq!(result.stop, Some(Stop::TestBound));
    assert_eq!(result.direct.len(), 3);
}

#[test]
fn seeded_trace_common_public_helpers_use_item_identity_and_private_helpers_stay_local() {
    let real = format!(
        "fn changed() {{}}\npub fn execute() {{\n    changed();\n}}\n{}",
        gate("other-01", "own", "execute();")
    );
    let fake = format!(
        "fn execute() {{}}\n{}",
        gate("other-02", "fake", "execute();")
    );
    let consumer = format!(
        "use crate::real::execute;\n{}",
        gate("other-02", "actual", "execute();")
    );
    let mut refs = index(&[
        ("crates/rsid/src/real.rs", &real),
        ("crates/rsid/src/fake.rs", &fake),
        ("crates/rsid/src/consumer.rs", &consumer),
    ]);
    let result = trace_tokens(
        &mut refs,
        &Seeds {
            changed: set(&["changed"]),
            ..Seeds::default()
        },
        Bounds::default(),
    );
    assert_eq!(result.stop, None);
    assert_eq!(
        result
            .tests
            .unwrap()
            .iter()
            .map(|o| o.name.clone())
            .collect::<BTreeSet<_>>(),
        set(&["own", "actual"])
    );
}

#[test]
fn seeded_trace_hubs_preserve_named_direct_observers_and_broad_fallback() {
    let a = format!(
        "impl Registry {{\n    fn execute() {{\n        changed();\n    }}\n}}\n{}{}",
        gate("other-01", "near", "changed();"),
        gate("other-01", "method", "Registry::execute();")
    );
    let b = "impl Other {\n    fn execute() {}\n}\nfn first(_: Registry) {}\nfn second(_: Registry) {}\n";
    let mut refs = index(&[("crates/rsid/src/a.rs", &a), ("crates/rsid/src/b.rs", b)]);
    let result = trace_tokens(
        &mut refs,
        &Seeds {
            changed: set(&["changed"]),
            types: set(&["Registry"]),
            ..Seeds::default()
        },
        Bounds {
            hub: 1,
            ..Bounds::default()
        },
    );
    assert_eq!(result.stop, Some(Stop::Hub));
    assert_eq!(result.tests, None);
    assert_eq!(
        result
            .direct
            .keys()
            .map(|o| o.name.clone())
            .collect::<BTreeSet<_>>(),
        set(&["near", "method"])
    );
    assert!(result.hubs["Registry"] > 1);
}

#[test]
fn unresolvable_seeded_trace_retains_settled_priorities_and_reports_identity_reason() {
    let a = format!(
        "pub fn execute() {{\n    changed();\n}}\n{}",
        gate("other-01", "near", "changed();")
    );
    let b = "pub fn execute() {}\nfn caller() {\n    crate::missing::execute();\n}\n";
    let mut refs = index(&[("crates/rsid/src/a.rs", &a), ("crates/rsid/src/b.rs", b)]);
    let result = trace_tokens(
        &mut refs,
        &Seeds {
            changed: set(&["changed"]),
            ..Seeds::default()
        },
        Bounds::default(),
    );
    assert_eq!(result.stop, Some(Stop::UnresolvedIdentity));
    assert_eq!(result.direct.len(), 1);
    assert!(result.diagnostic.unwrap().contains("no defining module"));
}

#[test]
fn mechanical_constructor_proofs_allow_only_inert_new_default_calls() {
    use super::seeds::*;
    assert_eq!(
        mechanical_types("Arc::new(Registry::default())"),
        Some(set(&["Registry"]))
    );
    assert_eq!(mechanical_types("Cell::new(None)"), Some(BTreeSet::new()));
    assert_eq!(
        mechanical_types("Registry::new(12)"),
        Some(set(&["Registry"]))
    );
    for value in [
        "Registry::new(compute())",
        "Registry::restore()",
        "Registry::new() + 1",
        "(Registry::new())",
        "Registry::new(1, 2)",
        "make_registry()",
    ] {
        assert_eq!(mechanical_types(value), None, "{value}");
    }
}

#[test]
fn private_field_proof_requires_one_private_declaration_and_mechanical_initializers() {
    use super::seeds::*;
    let texts = BTreeMap::from([("a.rs".into(), "struct State {\n    journal: Registry,\n}\nimpl State {\n    fn new() -> Self {\n        Self {\n            journal: Registry::new(),\n        }\n    }\n}\n".into())]);
    assert_eq!(
        unique_private_field_owner(&texts, "journal", &set(&["State"])).unwrap(),
        Some("State".into())
    );
    for extra in [
        "struct Other {\n    journal: Registry,\n}\n",
        "fn parameter(journal: Registry) {}\n",
        "macro_rules! fixture { () => { journal: Registry } }\n",
        "fn build() {\n    State {\n        journal: Registry::restore(),\n    }\n}\n",
        "struct Public {\n    pub journal: Registry,\n}\n",
    ] {
        let mut texts = texts.clone();
        texts.insert("b.rs".into(), extra.into());
        assert_eq!(
            unique_private_field_owner(&texts, "journal", &set(&["State", "Other", "Public"]))
                .unwrap(),
            None,
            "{extra}"
        );
    }
    assert_eq!(
        unique_private_field_owner(&texts, "journal", &BTreeSet::new()).unwrap(),
        None
    );
}

#[test]
fn initializer_proof_requires_the_owning_struct_literal_and_correct_self() {
    use super::seeds::*;
    let fields = BTreeMap::from([("journal".into(), "State".into())]);
    for (opener, enclosing, valid) in [
        ("Self {", Some("State"), true),
        ("State {", None, true),
        ("Other {", None, false),
        ("Self {", Some("Other"), false),
        ("let State {", None, false),
    ] {
        let text =
            format!("fn make() {{\n    {opener}\n        journal: Registry::new(),\n    }}\n}}");
        let rows: Vec<_> = text.lines().collect();
        assert_eq!(
            initializer_value(&rows, 2, &fields, enclosing).is_some(),
            valid,
            "{opener}"
        );
    }
}

#[test]
fn type_owner_and_unit_enum_proofs_keep_payloads_discriminants_and_variants_broad() {
    use super::seeds::*;
    let text = "pub enum State {\n    Ready,\n    Done,\n}\nimpl<T> Trait<T> for State {\n    fn name(&self) {}\n}\n";
    let types = set(&["State"]);
    assert_eq!(
        type_owners(text, &types).unwrap(),
        vec![Some("State".into()); 7]
    );
    assert_eq!(
        enum_item_lines(text, &types)
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    for variant in [
        "    Data(String),",
        "    Value = 2,",
        "    #[default]\n    Ready,",
    ] {
        assert!(
            enum_item_lines(&format!("enum State {{\n{variant}\n}}\n"), &types)
                .unwrap()
                .is_empty()
        );
    }
}

fn seed_proof(
    text: &str,
    lines: &[usize],
    frequency: BTreeMap<String, usize>,
    types: BTreeSet<String>,
) -> Result<super::seeds::Proof, String> {
    super::seeds::trace_seeds(
        text,
        &lines.iter().copied().collect(),
        &super::seeds::SeedContext {
            frequency: &frequency,
            parent: "session",
            types: &types,
            private_fields: &BTreeMap::new(),
            host: "linux",
        },
    )
}

#[test]
fn hunk_seed_proof_classifies_function_attributes_nested_spans_and_comments() {
    let text = "#[cfg(test)]\nfn helper() {\n    fn nested() {\n        changed();\n    }\n}\n/* A comment\n   \"nested braces }\"\n*/\n";
    assert_eq!(
        seed_proof(text, &[1], BTreeMap::new(), BTreeSet::new())
            .unwrap()
            .seeds
            .changed,
        set(&["helper"])
    );
    assert_eq!(
        seed_proof(text, &[4], BTreeMap::new(), BTreeSet::new())
            .unwrap()
            .seeds
            .changed,
        set(&["nested"])
    );
    let proof = seed_proof(text, &[7, 8, 9], BTreeMap::new(), BTreeSet::new()).unwrap();
    assert!(proof.seeds.changed.is_empty() && proof.seeds.tokens.is_empty());
    assert!(seed_proof("/* broken", &[1], BTreeMap::new(), BTreeSet::new()).is_err());
}

#[test]
fn large_production_hunks_use_rare_tokens_but_large_test_helpers_keep_function_identity() {
    let body = format!("{}    special_handler();\n", "    common();\n".repeat(64));
    let text = format!("fn dispatch() {{\n{body}}}\n");
    let proof = seed_proof(&text, &[66], BTreeMap::new(), BTreeSet::new()).unwrap();
    assert_eq!(proof.seeds.tokens, set(&["special_handler"]));
    assert_eq!(proof.seeds.changed, BTreeSet::new());
    let helper = format!("#[cfg(test)]\n{text}");
    assert_eq!(
        seed_proof(&helper, &[67], BTreeMap::new(), BTreeSet::new())
            .unwrap()
            .seeds
            .changed,
        set(&["dispatch"])
    );
    let common = BTreeMap::from([("special_handler".into(), 11)]);
    assert_eq!(
        seed_proof(&text, &[66], common, BTreeSet::new())
            .unwrap()
            .seeds
            .changed,
        set(&["dispatch"])
    );
}

#[test]
fn unit_enum_and_common_field_hunks_trace_the_unique_concrete_type() {
    let enums = "enum State {\n    Ready,\n    Done,\n}\n";
    let proof = seed_proof(
        enums,
        &[2, 3],
        BTreeMap::from([("State".into(), 5)]),
        set(&["State"]),
    )
    .unwrap();
    assert_eq!(proof.seeds.tokens, set(&["State"]));
    let fields = "pub struct Registry {\n    status: bool,\n}\n";
    let counts = BTreeMap::from([("status".into(), 80), ("Registry".into(), 50)]);
    assert_eq!(
        seed_proof(fields, &[2], counts, set(&["Registry"]))
            .unwrap()
            .seeds
            .tokens,
        set(&["Registry"])
    );
    assert!(
        seed_proof(
            "enum State {\n    Value(u8),\n}\n",
            &[2],
            BTreeMap::new(),
            set(&["State"])
        )
        .is_err()
    );
}

#[test]
fn module_and_reexport_hunks_resolve_only_explicit_rare_seeds() {
    let proof = seed_proof(
        "pub mod child;\npub use crate::real::Helper;\n",
        &[1, 2],
        BTreeMap::new(),
        BTreeSet::new(),
    )
    .unwrap();
    assert_eq!(proof.seeds.mounted, set(&["session::child"]));
    assert_eq!(proof.seeds.tokens, set(&["Helper"]));
    for declaration in [
        "pub use crate::real::*;",
        "pub use crate::real::{A, B};",
        "pub struct Registry {",
        "const UNRELATED: usize = 2;",
        "const FIXTURE: &str = \"pub use crate::real::Helper;\";",
    ] {
        assert!(
            seed_proof(declaration, &[1], BTreeMap::new(), BTreeSet::new()).is_err(),
            "{declaration}"
        );
    }
}

#[test]
fn old_and_new_hunk_seed_union_retains_deleted_and_added_observers() {
    use super::seeds::*;
    let path = "crates/rsid/src/session.rs";
    let old = tree(&[(path, "fn old() {\n    previous();\n}\n")]);
    let source = tree(&[(path, "fn new() {\n    current();\n}\n")]);
    let refs = ItemReferences::new(&source).unwrap();
    let hunks = vec![Hunk {
        old_first: 1,
        old_count: 3,
        new_first: 1,
        new_count: 3,
    }];
    assert_eq!(
        changed_seed_proof(&source, &old, &refs, path, &hunks)
            .unwrap()
            .seeds
            .changed,
        set(&["old", "new"])
    );
    let old = tree(&[(path, "const UNRELATED: usize = 2;\n")]);
    assert!(changed_seed_proof(&source, &old, &refs, path, &hunks).is_err());
}

#[test]
fn private_initializer_hunks_return_explicit_omission_obligations() {
    use super::seeds::*;
    let text = "struct State {\n    journal: Registry,\n}\nimpl State {\n    fn new() -> Self {\n        Self {\n            journal: Registry::new(),\n        }\n    }\n}\n";
    let context = SeedContext {
        frequency: &BTreeMap::new(),
        parent: "session",
        types: &set(&["State", "Registry"]),
        private_fields: &BTreeMap::from([("journal".into(), "State".into())]),
        host: "linux",
    };
    let proof = trace_seeds(text, &BTreeSet::from([7]), &context).unwrap();
    assert_eq!(proof.initializers, set(&["journal"]));
    assert_eq!(proof.seeds.tokens, set(&["journal", "Registry"]));
}

#[test]
fn seeded_token_observers_include_literal_predicate_readers_without_item_call_edges() {
    let text = format!(
        "fn reader() {{\n    source.contains(\"special_token\");\n}}\n{}",
        gate("other-01", "predicate", "reader();")
    );
    let mut refs = index(&[("crates/rsid/src/reader.rs", &text)]);
    let result = trace_tokens(
        &mut refs,
        &Seeds {
            tokens: set(&["special_token"]),
            ..Seeds::default()
        },
        Bounds::default(),
    );
    assert_eq!(result.stop, None);
    assert_eq!(
        result
            .tests
            .unwrap()
            .iter()
            .map(|o| o.name.clone())
            .collect::<BTreeSet<_>>(),
        set(&["predicate"])
    );
}

#[test]
fn multiline_quoted_functions_and_closing_braces_do_not_truncate_real_observers() {
    let text = "fn actual() {\n    let fixture = r#\"\npub fn imaginary() {}\n}\n\"#;\n    changed();\n}\n";
    let functions = rust_functions(text, "linux");
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].name, "actual");
    assert_eq!(functions[0].end, 7);
    assert!(functions[0].body.contains("changed();"));
    let refs = index(&[("crates/rsid/src/session.rs", text)]);
    assert_eq!(refs.functions.len(), 1);
}

#[test]
fn unchanged_large_inline_test_helpers_still_reach_runtime_observers() {
    let padding = "        unrelated();\n".repeat(64);
    let text = format!(
        "mod tests {{\n    fn helper() {{\n{padding}        changed();\n    }}\n    #[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-01\"))]\n    #[test]\n    fn observes() {{\n        helper();\n    }}\n}}\n"
    );
    let mut refs = index(&[("crates/rsid/src/session.rs", &text)]);
    let result = trace_tokens(
        &mut refs,
        &Seeds {
            tokens: set(&["changed"]),
            ..Seeds::default()
        },
        Bounds::default(),
    );
    assert_eq!(result.stop, None);
    assert_eq!(
        result
            .tests
            .unwrap()
            .iter()
            .map(|o| o.name.clone())
            .collect::<BTreeSet<_>>(),
        set(&["observes"])
    );
}
