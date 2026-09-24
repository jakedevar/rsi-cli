//! Real-corpus smoke test for the handoff validator.
//!
//! Walks `thoughts/shared/handoffs/general/*.md` and asserts that lenient
//! mode passes on ≥75% of the historical corpus, matching the field-set
//! survey numbers in research §Component E. Failed paths are printed for
//! triage but do not fail the test individually — this is a smoke gate,
//! not a per-file gate.

use std::path::PathBuf;

use rsi_common::handoff_schema::{ValidationMode, validate};

fn handoffs_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/rsi-common -> repo root -> thoughts/shared/handoffs/general
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("thoughts/shared/handoffs/general"))
        .expect("failed to derive corpus path from CARGO_MANIFEST_DIR")
}

#[test]
fn lenient_mode_accepts_seventy_five_percent_of_corpus() {
    let dir = handoffs_dir();
    if !dir.exists() {
        eprintln!(
            "corpus dir not found: {} — skipping smoke test",
            dir.display()
        );
        return;
    }

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();

    for entry in std::fs::read_dir(&dir).expect("read_dir") {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() < 100 {
            continue; // skip near-empty files
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        total += 1;
        let v = validate(&content, ValidationMode::Lenient);
        if v.valid {
            passed += 1;
        } else {
            let summary = v
                .errors
                .iter()
                .take(3)
                .map(|e| format!("{}:{}", e.field, e.rule))
                .collect::<Vec<_>>()
                .join(", ");
            failures.push((path, summary));
        }
    }

    if total == 0 {
        eprintln!("no .md handoffs found under {}; skipping", dir.display());
        return;
    }

    eprintln!(
        "corpus: {passed}/{total} valid in lenient mode ({}%)",
        (passed * 100) / total
    );
    if !failures.is_empty() {
        eprintln!("--- failures (first 20) ---");
        for (path, summary) in failures.iter().take(20) {
            eprintln!("  {}: {summary}", path.display());
        }
    }

    let threshold = (total as f64 * 0.75).ceil() as usize;
    assert!(
        passed >= threshold,
        "corpus pass rate {passed}/{total} below 75% threshold ({threshold})"
    );
}
