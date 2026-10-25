//! Private-field intent fixtures; no legacy executable is used as an oracle.
use super::{
    git::Hunk,
    identity::ItemReferences,
    seeds::{self, Proof},
    source::SourceTree,
    trace::{self, Bounds, Stop},
};
use std::collections::{BTreeMap, BTreeSet};

const PATH: &str = "crates/rsid-store/src/store/mod.rs";
const FIELD: &str = "    cache: RefCell<idle::IdleCache>,\n";
const INIT: &str = "            cache: RefCell::new(idle::IdleCache::default()),\n";
const STORE: &str = "pub mod idle;\nmod open_tests;\nmod users;\npub struct Store {\n    conn: u32,\n}\nimpl Store {\n    pub fn open() -> Store {\n        Store {\n            conn: 1,\n        }\n    }\n    pub fn open_for_test() -> Self {\n        Self {\n            conn: 2,\n        }\n    }\n}\n";

fn gate(name: &str, body: &str) -> String {
    format!(
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-01\"))]\n#[test]\nfn {name}() {{\n    {body}\n}}\n"
    )
}
fn tree(text: &str, extra: &str) -> SourceTree {
    SourceTree::new(
        BTreeMap::from([
            (
                "crates/rsid-store/src/lib.rs".into(),
                "pub mod store;\n".into(),
            ),
            (PATH.into(), text.into()),
            (
                "crates/rsid-store/src/store/idle.rs".into(),
                format!(
                    "#[derive(Default)]\npub struct IdleCache {{\n    pub hits: u32,\n}}\nimpl Store {{\n    pub fn peek(&self) -> u32 {{\n        self.cache.borrow().hits\n    }}\n}}\n{}",
                    gate("idle_peek", "Store::open().peek();")
                ),
            ),
            (
                "crates/rsid-store/src/store/users.rs".into(),
                (0..160)
                    .map(|n| format!("fn user_{n}(_store: &Store) {{}}\n"))
                    .collect(),
            ),
            (
                "crates/rsid-store/src/store/open_tests.rs".into(),
                (0..70)
                    .map(|n| gate(&format!("opens_{n}"), "Store::open();"))
                    .collect(),
            ),
            ("crates/rsid-store/src/store/extra.rs".into(), extra.into()),
        ]),
        "linux",
    )
}
fn candidate() -> String {
    STORE
        .replace("    conn: u32,\n", &format!("    conn: u32,\n{FIELD}"))
        .replace(
            "            conn: 1,\n",
            &format!("            conn: 1,\n{INIT}"),
        )
        .replace(
            "            conn: 2,\n",
            &format!("            conn: 2,\n{INIT}"),
        )
}
fn additions(text: &str) -> Vec<Hunk> {
    text.lines()
        .enumerate()
        .filter(|(_, r)| r.trim_start().starts_with("cache:"))
        .enumerate()
        .map(|(offset, (i, _))| Hunk {
            old_first: i - offset,
            old_count: 0,
            new_first: i + 1,
            new_count: 1,
        })
        .collect()
}
fn proof(text: &str, extra: &str) -> (ItemReferences, Proof) {
    let source = tree(text, extra);
    let index = ItemReferences::new(&source).unwrap();
    let proof =
        seeds::changed_seed_proof(&source, &tree(STORE, extra), &index, PATH, &additions(text))
            .unwrap();
    (index, proof)
}

#[test]
fn changed_field_map_and_initializer_omission_select_the_reader_at_a_hub_type() {
    let (mut index, proof) = proof(&candidate(), "");
    assert_eq!(
        proof.private_fields,
        BTreeMap::from([("cache".into(), "Store".into())])
    );
    assert_eq!(proof.initializers, BTreeSet::from(["cache".into()]));
    assert_eq!(proof.seeds.changed, BTreeSet::new());
    let trace = trace::trace_proof(&mut index, &proof, Bounds::default()).unwrap();
    assert_eq!(trace.stop, None);
    assert_eq!(
        trace
            .tests
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect::<Vec<_>>(),
        ["idle_peek"]
    );
    assert_eq!(trace.direct.values().copied().collect::<Vec<_>>(), [2]);
}

#[test]
fn field_word_view_is_local_to_one_trace_and_keeps_raw_frequencies() {
    let (mut index, proof) = proof(&candidate(), "");
    let before: Vec<_> = index.functions.iter().map(|f| f.words.clone()).collect();
    let frequency = index.frequency.clone();
    trace::trace_proof(&mut index, &proof, Bounds::default()).unwrap();
    assert_eq!(
        index
            .functions
            .iter()
            .map(|f| f.words.clone())
            .collect::<Vec<_>>(),
        before
    );
    assert_eq!(index.frequency, frequency);
    let unomitted = trace::trace_tokens(&mut index, &proof.seeds, Bounds::default());
    assert_eq!(unomitted.stop, Some(Stop::TestBound));
    assert_eq!(unomitted.tests, None);
    assert_eq!(unomitted.direct.len(), 71);
}

#[test]
fn constructors_that_read_write_or_scan_the_field_keep_their_observers() {
    for mention in [
        "let seen = self.cache.borrow();",
        "self.cache = changed();",
        "let Store { cache: seen, .. } = other;",
        "// cache is read by this source scanner",
        "let fixture = \"cache\";",
    ] {
        let text = candidate().replace(
            "        Store {",
            &format!("        {mention}\n        Store {{"),
        );
        let (mut index, proof) = proof(&text, "");
        let trace = trace::trace_proof(&mut index, &proof, Bounds::default()).unwrap();
        assert_eq!(trace.stop, Some(Stop::TestBound), "{mention}");
        assert_eq!(trace.tests, None);
        assert_eq!(trace.direct.len(), 71);
    }
}

#[test]
fn a_constructor_type_mentioned_outside_the_initializer_remains_an_observer() {
    let text = candidate().replace(
        "        Store {",
        "        let cache_type = IdleCache::default();\n        Store {",
    );
    let (mut index, proof) = proof(&text, "");
    let trace = trace::trace_proof(&mut index, &proof, Bounds::default()).unwrap();
    assert_eq!(trace.stop, Some(Stop::TestBound));
    assert_eq!(trace.direct.len(), 71);
}

#[test]
fn invalid_private_ownership_keeps_common_field_declarations_unresolved() {
    let readers: String = (0..12)
        .map(|n| format!("fn read_{n}(s: &Store) {{ s.cache.read(); }}\n"))
        .collect();
    for extra in [
        "pub struct Other {\n    cache: u8,\n}\n",
        "macro_rules! field {\n    ($name:ident) => {\n        cache: 0,\n    };\n}\n",
        "fn take(store: Store) {\n    let Store {\n        cache: seen,\n        ..\n    } = store;\n}\n",
        "fn parameter(\n    cache: u32,\n) {}\n",
    ] {
        let extra = format!("{readers}{extra}");
        let source = tree(&candidate(), &extra);
        let index = ItemReferences::new(&source).unwrap();
        assert!(
            seeds::changed_seed_proof(
                &source,
                &tree(STORE, &extra),
                &index,
                PATH,
                &additions(&candidate())
            )
            .is_err()
        );
    }
    let text = candidate().replace(FIELD, "    pub cache: RefCell<idle::IdleCache>,\n");
    let source = tree(&text, &readers);
    let index = ItemReferences::new(&source).unwrap();
    let hunks: Vec<_> = additions(&candidate());
    assert!(
        seeds::changed_seed_proof(&source, &tree(STORE, &readers), &index, PATH, &hunks).is_err()
    );
}

#[test]
fn a_nonmechanical_initializer_preserves_constructor_coverage() {
    let text = candidate().replace(
        "RefCell::new(idle::IdleCache::default())",
        "RefCell::new(warm_up(1))",
    );
    let (mut index, proof) = proof(&text, "");
    assert_eq!(proof.private_fields, BTreeMap::new());
    assert_eq!(proof.initializers, BTreeSet::new());
    assert_eq!(
        proof.seeds.changed,
        BTreeSet::from(["open".into(), "open_for_test".into()])
    );
    let trace = trace::trace_proof(&mut index, &proof, Bounds::default()).unwrap();
    assert_eq!(trace.tests, None);
    assert_eq!(trace.stop, Some(Stop::TestBound));
}

#[test]
fn initializer_omission_requires_an_explicit_ownership_obligation() {
    let (index, mut proof) = proof(&candidate(), "");
    proof.private_fields.clear();
    assert_eq!(
        seeds::observer_words(&index, &proof).unwrap_err(),
        "initializer omission has no private-field ownership proof"
    );
}

#[test]
fn initializer_words_in_quoted_source_and_comments_are_still_readers() {
    for text in [
        "fn source_reader() {\n    let fixture = r#\"Store {\n        cache: IdleCache::default(),\n    }\"#;\n}\n",
        "fn source_reader() {\n    /* Store {\n        cache: IdleCache::default(),\n    } */\n}\n",
    ] {
        let (index, proof) = proof(&candidate(), text);
        let words = seeds::observer_words(&index, &proof).unwrap();
        let i = index
            .functions
            .iter()
            .position(|f| f.function.name == "source_reader")
            .unwrap();
        assert_eq!(words[i], index.functions[i].words);
    }
}

#[test]
fn field_proof_reads_the_pinned_candidate_including_unchanged_sources() {
    let extra = "const FIXTURE: &str = r#\"struct Other {\n    cache: u8,\n}\"#;\n";
    let (_, proof) = proof(&candidate(), extra);
    assert_eq!(
        proof.private_fields,
        BTreeMap::from([("cache".into(), "Store".into())])
    );
}
