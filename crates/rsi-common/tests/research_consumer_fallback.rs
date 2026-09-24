//! Consumer-side fallback regression test (RSI-014 Phase 4).
//!
//! Locks the property that `probe_or_fallback` returns `MarkdownFallback`
//! whenever the JSON sidecar is missing OR invalid, and `JsonValid` only
//! when the JSON parses AND validates against v1. This is the safety net
//! that keeps the 197 legacy markdown-only research docs working.

use std::fs;
use std::path::{Path, PathBuf};

use rsi_common::research_schema::{ResearchSource, probe_or_fallback};

fn write_md(dir: &Path) -> PathBuf {
    let p = dir.join("legacy.md");
    fs::write(
        &p,
        "---\nstatus: complete\n---\n\n# Research: legacy\n\n## Research Question\n\
         How do we keep legacy docs working?\n",
    )
    .expect("write md");
    p
}

#[test]
fn legacy_markdown_only_doc_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let md = write_md(dir.path());
    match probe_or_fallback(&md) {
        ResearchSource::MarkdownFallback(p) => assert_eq!(p, md),
        ResearchSource::JsonValid(_) => {
            panic!("expected MarkdownFallback for legacy md-only doc")
        }
        _ => panic!("unexpected ResearchSource variant"),
    }
}

#[test]
fn valid_sidecar_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let md = write_md(dir.path());
    let json = md.with_extension("json");
    fs::write(
        &json,
        r#"{
            "version": 1,
            "research_question": "How do we keep legacy docs working?",
            "areas": ["common"],
            "findings": [
                {"summary": "MarkdownFallback path stays alive.", "file_ref": "crates/rsi-common/src/research_schema/mod.rs:1", "confidence": "medium"}
            ],
            "file_refs": [
                {"path": "crates/rsi-common/src/research_schema/mod.rs", "lines": [1, 50]}
            ],
            "open_questions": []
        }"#,
    )
    .unwrap();
    match probe_or_fallback(&md) {
        ResearchSource::JsonValid(doc) => {
            assert_eq!(doc.research_question, "How do we keep legacy docs working?");
            assert_eq!(doc.areas, vec!["common".to_string()]);
            assert_eq!(doc.findings.len(), 1);
        }
        ResearchSource::MarkdownFallback(_) => panic!("expected JsonValid"),
        _ => panic!("unexpected ResearchSource variant"),
    }
}

#[test]
fn schema_invalid_sidecar_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let md = write_md(dir.path());
    let json = md.with_extension("json");
    // Empty areas violates Presence rule.
    fs::write(
        &json,
        r#"{
            "version": 1,
            "research_question": "Q?",
            "areas": [],
            "findings": [],
            "file_refs": [],
            "open_questions": []
        }"#,
    )
    .unwrap();
    match probe_or_fallback(&md) {
        ResearchSource::MarkdownFallback(p) => assert_eq!(p, md),
        ResearchSource::JsonValid(_) => panic!("expected fallback on invalid sidecar"),
        _ => panic!("unexpected ResearchSource variant"),
    }
}

#[test]
fn parse_error_sidecar_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let md = write_md(dir.path());
    let json = md.with_extension("json");
    fs::write(&json, "}{ broken").unwrap();
    match probe_or_fallback(&md) {
        ResearchSource::MarkdownFallback(p) => assert_eq!(p, md),
        ResearchSource::JsonValid(_) => panic!("expected fallback on parse error"),
        _ => panic!("unexpected ResearchSource variant"),
    }
}

#[test]
fn future_schema_version_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let md = write_md(dir.path());
    let json = md.with_extension("json");
    // version: 99 is far beyond the supported range → fails VersionPin (consumer speaks v2), forcing markdown fallback.
    fs::write(
        &json,
        r#"{
            "version": 99,
            "research_question": "Q?",
            "areas": ["A"],
            "findings": [],
            "file_refs": [],
            "open_questions": []
        }"#,
    )
    .unwrap();
    match probe_or_fallback(&md) {
        ResearchSource::MarkdownFallback(p) => assert_eq!(p, md),
        ResearchSource::JsonValid(_) => panic!("expected fallback on future version"),
        _ => panic!("unexpected ResearchSource variant"),
    }
}
