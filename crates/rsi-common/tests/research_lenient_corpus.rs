//! Smoke-test the `rsi-research-validate --lenient` floor against the
//! historical research-doc corpus under `thoughts/shared/research/`.
//!
//! `#[ignore]`-gated: this test reads from disk outside the cargo target
//! tree and is intended for on-demand verification, not the regular CI
//! cycle. Run with `cargo test -p rsi-common --test research_lenient_corpus
//! -- --ignored --nocapture`.
//!
//! Pass criterion: ≥70% of `*.json` sidecars validate clean in lenient
//! mode. The threshold is the RSI-021 ticket's success criterion (line 82).
//! If the corpus is empty (no JSON sidecars yet — RSI-014 just merged),
//! the test passes with an informational println. This is the expected
//! state on first runs after RSI-014 lands; the test is intended to
//! catch regressions once writer skills produce sidecars at scale.

use std::path::PathBuf;

use rsi_common::research_schema::{ValidationMode, validate_research_json_with_mode};

const PASS_RATE_THRESHOLD: f64 = 0.70;

fn corpus_root() -> PathBuf {
    // Walk up from CARGO_MANIFEST_DIR (crates/rsi-common) to the workspace
    // root, then into thoughts/shared/research. Worktrees inherit the
    // thoughts/ tree from main, so this works inside any worktree.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("thoughts/shared/research"))
        .expect("workspace root resolves from CARGO_MANIFEST_DIR")
}

#[test]
#[ignore]
fn lenient_floor_holds_for_at_least_seventy_percent_of_corpus() {
    let root = corpus_root();
    if !root.exists() {
        println!(
            "corpus root {:?} missing; skipping (no regression possible)",
            root
        );
        return;
    }

    let entries: Vec<PathBuf> = walk_json(&root);
    if entries.is_empty() {
        println!(
            "corpus root {:?} contains zero *.json sidecars — test passes vacuously. \
             This is expected immediately after RSI-014 merges (no writer skill has \
             produced sidecars yet); re-run once the corpus is non-empty.",
            root
        );
        return;
    }

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for path in &entries {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                failures.push((path.clone(), format!("io: {e}")));
                total += 1;
                continue;
            }
        };
        total += 1;
        let v = validate_research_json_with_mode(&content, ValidationMode::Lenient);
        if v.valid {
            passed += 1;
        } else {
            let err_summary = v
                .errors
                .iter()
                .map(|e| format!("{}/{}", e.field, e.rule))
                .collect::<Vec<_>>()
                .join(",");
            failures.push((path.clone(), err_summary));
        }
    }

    let rate = passed as f64 / total as f64;
    println!(
        "lenient corpus pass rate: {}/{} = {:.1}% (threshold {:.0}%)",
        passed,
        total,
        rate * 100.0,
        PASS_RATE_THRESHOLD * 100.0
    );
    if rate < PASS_RATE_THRESHOLD {
        for (p, why) in failures.iter().take(20) {
            println!("  FAIL {:?}: {}", p, why);
        }
        panic!(
            "lenient floor below {:.0}% — see printed failures",
            PASS_RATE_THRESHOLD * 100.0
        );
    }
}

fn walk_json(dir: &std::path::Path) -> Vec<PathBuf> {
    // Operational state files (Phase 4 cron caches, etc.) live alongside
    // research sidecars but are NOT v1 research docs. Skip them by exact
    // filename so the corpus check stays tight.
    const OPERATIONAL_STATE_FILES: &[&str] = &[
        "stale-branch-issues.json", // Phase 4.4 stale-branch alarm dedup cache
    ];

    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk_json(&p));
        } else if p.extension().is_some_and(|e| e == "json") {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or_default();
            if OPERATIONAL_STATE_FILES.contains(&name) {
                continue;
            }
            out.push(p);
        }
    }
    out
}
