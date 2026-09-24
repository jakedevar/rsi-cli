//! Smoke test that walks the on-repo `thoughts/shared/research/*.json`
//! corpus and asserts every present sidecar validates against the current
//! `RESEARCH_SCHEMA_VERSION` pin (v2). v2 is a strict superset of v1, so
//! pre-v2 sidecars (no `Finding.id`) still validate green under this pin.
//!
//! The corpus starts empty; this test passes vacuously until the first JSON
//! sidecar lands. Once non-empty, the test guards drift — any future commit
//! that introduces a schema-violating sidecar fails CI here.

use std::path::PathBuf;

use rsi_common::research_schema::validate_research_json;

fn corpus_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is `crates/rsi-common`. Climb to repo root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("thoughts/shared/research"))
        .expect("manifest parent path resolves")
}

#[test]
fn every_research_json_sidecar_validates() {
    let dir = corpus_dir();
    if !dir.exists() {
        eprintln!(
            "research dir {} does not exist — corpus test passes vacuously",
            dir.display()
        );
        return;
    }

    // Operational state files (Phase 4 cron caches, etc.) live alongside
    // research sidecars but are NOT v1 research docs. Skip them by exact
    // filename so the corpus check stays tight.
    const OPERATIONAL_STATE_FILES: &[&str] = &[
        "stale-branch-issues.json", // Phase 4.4 stale-branch alarm dedup cache
    ];

    let mut sidecars: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read research dir") {
        let entry = entry.expect("read entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if OPERATIONAL_STATE_FILES.contains(&name) {
            continue;
        }
        sidecars.push(path);
    }

    if sidecars.is_empty() {
        eprintln!("no JSON sidecars yet — corpus test passes vacuously");
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    for path in &sidecars {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{}: read error: {}", path.display(), e));
                continue;
            }
        };
        let v = validate_research_json(&content);
        if !v.valid {
            let first = v
                .errors
                .first()
                .map(|e| format!("{} [{}]: {}", e.field, e.rule, e.message))
                .unwrap_or_else(|| "<no first error>".to_string());
            failures.push(format!("{}: {}", path.display(), first));
        }
    }

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("corpus failure: {}", f);
        }
        panic!("{} sidecar(s) failed validation", failures.len());
    }
}
