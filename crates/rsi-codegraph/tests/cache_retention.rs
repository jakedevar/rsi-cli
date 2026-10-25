//! #3491: the extraction cache keeps raw facts only for corpora that fit its
//! budget, and every policy yields exactly the facts a fresh cache would.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Focused acceptance fixture.

use rsi_codegraph::SourceFile;
use rsi_codegraph::invalidate::{
    DEFAULT_RETAIN_BUDGET_BYTES, ExtractionCache, OTHER_FACT_HEAP_PER_SOURCE_BYTE,
    RUST_FACT_HEAP_PER_SOURCE_BYTE, estimated_fact_heap_bytes,
};

fn source(path: &str, text: &str) -> SourceFile {
    SourceFile {
        relative_path: path.into(),
        bytes: text.as_bytes().to_vec(),
    }
}

fn corpus() -> Vec<SourceFile> {
    vec![
        source(
            "src/lib.rs",
            "fn caller() { crate::b::target(); crate::b::target(); }\n",
        ),
        source("src/b.rs", "pub fn target() {}\n"),
        source(
            "notes/plan.md",
            "---\ntitle: one\n---\n# Plan\nsatisfies: [F-001]\n",
        ),
        source("notes/findings.md", "# Findings\n## F-001 Evidence\n"),
    ]
}

#[test]
fn the_estimate_weighs_rust_heavier_than_other_files() {
    assert_eq!(
        estimated_fact_heap_bytes("src/lib.rs", 100),
        100 * RUST_FACT_HEAP_PER_SOURCE_BYTE
    );
    assert_eq!(
        estimated_fact_heap_bytes("notes/plan.md", 100),
        100 * OTHER_FACT_HEAP_PER_SOURCE_BYTE
    );
    assert!(RUST_FACT_HEAP_PER_SOURCE_BYTE > OTHER_FACT_HEAP_PER_SOURCE_BYTE);
}

#[test]
fn a_small_corpus_is_retained_and_a_large_one_is_not() {
    let mut small = ExtractionCache::new();
    small.update(corpus()).unwrap();
    assert!(small.retains_raw_facts());

    let mut over_budget = ExtractionCache::with_retain_budget(8);
    over_budget.update(corpus()).unwrap();
    assert!(!over_budget.retains_raw_facts());

    // The default budget is one copy of the facts of tens of MB of source: the
    // hub's 125 MB (Rust) and 193 MB (mixed) checkouts exceed it, a project of a
    // few MB of source does not.
    let hub_checkout = estimated_fact_heap_bytes("src/lib.rs", 125 * 1024 * 1024);
    assert!(hub_checkout > DEFAULT_RETAIN_BUDGET_BYTES);
    let project = estimated_fact_heap_bytes("src/lib.rs", 4 * 1024 * 1024);
    assert!(project <= DEFAULT_RETAIN_BUDGET_BYTES);
}

#[test]
fn every_retention_policy_matches_a_fresh_cache_across_edits() {
    let base = corpus();
    let edits = [
        base.clone(),
        vec![
            source("src/lib.rs", "fn caller() { crate::b::target(); }\n"),
            base[1].clone(),
            base[2].clone(),
            base[3].clone(),
        ],
        vec![base[0].clone(), base[2].clone(), base[3].clone()],
        vec![
            base[0].clone(),
            base[1].clone(),
            base[2].clone(),
            source("notes/findings.md", "# Findings\n## F-002 Evidence\n"),
        ],
        base.clone(),
    ];
    for budget in [0, 8, DEFAULT_RETAIN_BUDGET_BYTES] {
        let mut cache = ExtractionCache::with_retain_budget(budget);
        for files in &edits {
            let update = cache.update(files.clone()).unwrap();
            let clean = ExtractionCache::with_retain_budget(0)
                .update(files.clone())
                .unwrap();
            assert_eq!(update.facts, clean.facts, "budget {budget}");
            assert_eq!(update.inventory.digest, clean.inventory.digest);
        }
    }
}

#[test]
fn changed_owners_come_from_digests_so_an_unretained_cache_still_reports_exactly_the_edit() {
    let mut cache = ExtractionCache::with_retain_budget(0);
    let first = cache.update(corpus()).unwrap();
    assert_eq!(
        first.changed_owners.len(),
        4,
        "a first pass changes every owner"
    );
    assert!(!cache.retains_raw_facts());

    let unchanged = cache.update(corpus()).unwrap();
    assert!(unchanged.changed_owners.is_empty());

    let mut edited = corpus();
    edited[1] = source("src/b.rs", "pub fn target() { let _ = 1; }\n");
    let update = cache.update(edited).unwrap();
    assert_eq!(
        update
            .changed_owners
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["src/b.rs"]
    );
    assert!(update.invalidated_owners.contains("src/lib.rs"));

    let mut dropped = corpus();
    dropped.remove(1);
    let update = cache.update(dropped).unwrap();
    assert!(update.changed_owners.contains("src/b.rs"));
}

#[test]
fn a_rejected_inventory_leaves_a_cache_that_still_produces_correct_facts() {
    let mut cache = ExtractionCache::new();
    cache.update(corpus()).unwrap();
    let mut duplicate = corpus();
    duplicate.push(source("src/b.rs", "pub fn other() {}\n"));
    assert!(cache.update(duplicate).is_err());
    let after = cache.update(corpus()).unwrap();
    let clean = ExtractionCache::new().update(corpus()).unwrap();
    assert_eq!(after.facts, clean.facts);
}
