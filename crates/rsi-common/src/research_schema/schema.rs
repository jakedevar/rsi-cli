//! Typed v1 schema for `<doc>.json` research sidecars.
//!
//! `#[serde(deny_unknown_fields)]` on every struct guarantees that v2 fields
//! are rejected by a v1 binary — the consumer skill sees that as exit-2 and
//! falls back to markdown. See plan §Open question Q6.

use serde::{Deserialize, Serialize};

/// Top-level research-doc JSON shape (v1).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchDoc {
    pub version: u32,
    pub research_question: String,
    pub areas: Vec<String>,
    pub findings: Vec<Finding>,
    pub file_refs: Vec<FileRef>,
    pub open_questions: Vec<OpenQuestion>,
}

/// One synthesized finding from the research run.
///
/// `id` (v2, additive) is an optional provenance join key. Downstream stages
/// (plan-item `satisfies`/`covers`, source attribution) reference a finding by
/// this id. It is a strict-superset addition: v1 docs omit it (deserializes to
/// `None`), and when `None` it is skipped on serialization so v1 output stays
/// byte-stable. `#[serde(deny_unknown_fields)]` still rejects any *other*
/// unknown key.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    /// Optional provenance join key (v2). Absent in v1 docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub summary: String,
    pub file_ref: String,
    pub confidence: Confidence,
}

/// A file:line reference produced by the research run.
///
/// `lines` is a 2-element tuple `[start, end]`. Single-line refs encode as
/// `[N, N]`. Serde rejects arrays whose length is not exactly 2.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileRef {
    pub path: String,
    pub lines: [u32; 2],
}

/// One unresolved question surfaced during research.
///
/// `blocks_planning: true` means `/plan` MUST resolve this before
/// finalizing the plan; `false` means it can be deferred.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenQuestion {
    pub summary: String,
    pub blocks_planning: bool,
}

/// Confidence enum for a finding. v1 emits `Medium` uniformly; v2 may wire a
/// real heuristic (cross-agent agreement → `High`, single-agent thoughts-only
/// → `Low`).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_minimal() -> &'static str {
        r#"{
            "version": 1,
            "research_question": "Q?",
            "areas": ["A"],
            "findings": [],
            "file_refs": [],
            "open_questions": []
        }"#
    }

    #[test]
    fn valid_minimal_round_trips() {
        let doc: ResearchDoc = serde_json::from_str(valid_minimal()).unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.areas, vec!["A".to_string()]);
    }

    #[test]
    fn deny_unknown_fields_rejects_drift_at_root() {
        let bad = r#"{
            "version": 1,
            "research_question": "Q?",
            "areas": ["A"],
            "findings": [],
            "file_refs": [],
            "open_questions": [],
            "extra_v2_field": "nope"
        }"#;
        assert!(serde_json::from_str::<ResearchDoc>(bad).is_err());
    }

    #[test]
    fn deny_unknown_fields_rejects_drift_on_open_question() {
        let bad = r#"{
            "version": 1,
            "research_question": "Q?",
            "areas": ["A"],
            "findings": [],
            "file_refs": [],
            "open_questions": [
                {"summary": "x", "blocks_planning": false, "failure_mode": "v2-field"}
            ]
        }"#;
        assert!(serde_json::from_str::<ResearchDoc>(bad).is_err());
    }

    #[test]
    fn confidence_lowercase_only() {
        let bad = r#"{
            "version": 1,
            "research_question": "Q?",
            "areas": ["A"],
            "findings": [
                {"summary": "f", "file_ref": "src/lib.rs:1", "confidence": "High"}
            ],
            "file_refs": [],
            "open_questions": []
        }"#;
        assert!(serde_json::from_str::<ResearchDoc>(bad).is_err());
    }

    // ---- v2 `Finding.id` provenance-key coverage (S5) ----

    /// A v1-style finding: no `id` key at all. Must deserialize (id → None).
    fn finding_without_id_json() -> &'static str {
        r#"{"summary": "f", "file_ref": "src/lib.rs:1", "confidence": "medium"}"#
    }

    /// A v2 finding carrying the optional provenance `id`.
    fn finding_with_id_json() -> &'static str {
        r#"{"id": "F-001", "summary": "f", "file_ref": "src/lib.rs:1", "confidence": "medium"}"#
    }

    #[test]
    fn finding_without_id_deserializes_to_none() {
        let f: Finding = serde_json::from_str(finding_without_id_json()).unwrap();
        assert_eq!(f.id, None);
        assert_eq!(f.summary, "f");
    }

    #[test]
    fn finding_with_id_round_trips() {
        let f: Finding = serde_json::from_str(finding_with_id_json()).unwrap();
        assert_eq!(f.id.as_deref(), Some("F-001"));
        // serialize → deserialize → serialize is unchanged (byte-stable).
        let once = serde_json::to_string(&f).unwrap();
        let f2: Finding = serde_json::from_str(&once).unwrap();
        let twice = serde_json::to_string(&f2).unwrap();
        assert_eq!(once, twice);
        assert!(once.contains(r#""id":"F-001""#));
    }

    #[test]
    fn finding_without_id_serializes_without_id_key() {
        // skip_serializing_if keeps v1 output byte-stable: no `"id"` key emitted
        // when the field is None, and the value survives a re-parse unchanged.
        let f: Finding = serde_json::from_str(finding_without_id_json()).unwrap();
        let once = serde_json::to_string(&f).unwrap();
        assert!(
            !once.contains("\"id\""),
            "None id must not serialize: {once}"
        );
        let f2: Finding = serde_json::from_str(&once).unwrap();
        let twice = serde_json::to_string(&f2).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn finding_still_rejects_bogus_unknown_key() {
        // deny_unknown_fields must still fire for a genuine unknown key even
        // after `id` was added.
        let bad =
            r#"{"summary": "f", "file_ref": "src/lib.rs:1", "confidence": "medium", "bogus": 1}"#;
        assert!(serde_json::from_str::<Finding>(bad).is_err());
    }

    /// PINNING TEST (schema level): a v1 research doc — NO `id` anywhere — still
    /// deserializes under the v2-capable schema, AND a doc whose finding carries
    /// `id` round-trips serialize→deserialize→serialize unchanged.
    #[test]
    fn v1_doc_without_id_deserializes_and_v2_id_finding_round_trips() {
        // (a) A full v1 doc with an id-less finding still deserializes.
        let v1_doc = r#"{
            "version": 1,
            "research_question": "Q?",
            "areas": ["A"],
            "findings": [
                {"summary": "f", "file_ref": "src/lib.rs:1", "confidence": "medium"}
            ],
            "file_refs": [],
            "open_questions": []
        }"#;
        let doc: ResearchDoc = serde_json::from_str(v1_doc).unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.findings[0].id, None);

        // (b) A finding carrying `id` round-trips byte-stable.
        let f: Finding = serde_json::from_str(finding_with_id_json()).unwrap();
        let once = serde_json::to_string(&f).unwrap();
        let round: Finding = serde_json::from_str(&once).unwrap();
        let twice = serde_json::to_string(&round).unwrap();
        assert_eq!(once, twice);
        assert_eq!(round.id.as_deref(), Some("F-001"));
    }

    #[test]
    fn lines_must_be_two_element_tuple() {
        let bad = r#"{
            "version": 1,
            "research_question": "Q?",
            "areas": ["A"],
            "findings": [],
            "file_refs": [
                {"path": "src/lib.rs", "lines": [1, 2, 3]}
            ],
            "open_questions": []
        }"#;
        assert!(serde_json::from_str::<ResearchDoc>(bad).is_err());
    }
}
