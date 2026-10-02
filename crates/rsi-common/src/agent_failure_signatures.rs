//! `AgentQueryFailureSignatures` (#1016 acceptance 3): a read-only, typed query
//! over the known-failure signature records that live in open Issues.
//!
//! The result carries only typed signature records and their owner Issue's
//! identity and state, never an Issue body. Expiry (acceptance 4) is live:
//! the daemon offers only `Open`/`InProgress`, non-archived Issues, and
//! [`AgentQueryFailureSignaturesResultV1::from_issues`] drops any other status.

use crate::failure_signature::{SignatureRecord, parse_issue_blocks, status_is_live};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The fence info string the daemon prefilters Issue bodies on.
pub const FAILURE_SIGNATURE_BODY_MARKER: &str = crate::failure_signature::FENCE_INFO;

/// Stable refusal code for a malformed query.
pub const FAILURE_SIGNATURE_QUERY_INVALID: &str = "failure_signature_query_invalid";

/// Strict query: at least one of `test_id` and `digest`; both must match when
/// both are set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentQueryFailureSignaturesRequestV1 {
    /// Exact libtest/nextest test id.
    #[serde(default)]
    pub test_id: Option<String>,
    /// 64 lowercase hex characters: `failure_signature::digest` of the failure.
    #[serde(default)]
    pub digest: Option<String>,
}

impl AgentQueryFailureSignaturesRequestV1 {
    /// # Errors
    ///
    /// Returns a stable message when neither field is set, a field is empty or
    /// oversized, or `digest` is not 64 lowercase hex characters.
    pub fn validate(&self) -> Result<(), String> {
        if self.test_id.is_none() && self.digest.is_none() {
            return Err("set test_id, digest or both".to_string());
        }
        if let Some(test_id) = &self.test_id
            && (test_id.trim().is_empty() || test_id.len() > 512 || test_id.contains('\0'))
        {
            return Err("test_id must be 1..=512 NUL-free bytes".to_string());
        }
        if let Some(digest) = &self.digest
            && !(digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            return Err("digest must be 64 lowercase hex characters".to_string());
        }
        Ok(())
    }

    fn matches(&self, record: &SignatureRecord) -> bool {
        let test_ok = self
            .test_id
            .as_deref()
            .is_none_or(|test_id| record.test_id == test_id);
        let digest_ok = self
            .digest
            .as_deref()
            .is_none_or(|digest| record.matcher.digest.as_deref() == Some(digest));
        test_ok && digest_ok
    }
}

/// One matching record with its owner Issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureSignatureMatchV1 {
    pub record: SignatureRecord,
    /// The owner Issue's row id.
    pub issue_id: Uuid,
    /// The owner Issue's status; always `Open` or `InProgress` here.
    pub issue_status: String,
}

/// Typed result. An unknown query is an empty `records`, not an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentQueryFailureSignaturesResultV1 {
    pub records: Vec<FailureSignatureMatchV1>,
    /// Open Issues whose signature blocks did not parse. Their records are
    /// invisible to this query until the Issue body is fixed.
    pub malformed_issues: Vec<u64>,
}

/// One Issue offered to the query.
#[derive(Debug, Clone)]
pub struct SignatureIssueSource {
    pub issue_id: Uuid,
    pub display_number: u64,
    pub status: String,
    pub body: String,
}

impl AgentQueryFailureSignaturesResultV1 {
    /// Parse the signature blocks of every live Issue and keep the records
    /// that match `request`, ordered by owner Issue number then block order.
    #[must_use]
    pub fn from_issues(
        issues: &[SignatureIssueSource],
        request: &AgentQueryFailureSignaturesRequestV1,
    ) -> Self {
        let mut result = Self::default();
        for issue in issues {
            if !status_is_live(&issue.status) {
                continue;
            }
            match parse_issue_blocks(&issue.body, issue.display_number) {
                Ok(records) => {
                    result.records.extend(
                        records
                            .into_iter()
                            .filter(|record| request.matches(record))
                            .map(|record| FailureSignatureMatchV1 {
                                record,
                                issue_id: issue.issue_id,
                                issue_status: issue.status.clone(),
                            }),
                    );
                }
                Err(_) => result.malformed_issues.push(issue.display_number),
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::failure_signature::{FENCE_INFO, digest};

    const TEXT: &str = "thread 'a::b' (1) panicked at crates/x.rs:1:1:\nboom 42\nnote: run with `RUST_BACKTRACE=1`";

    fn body(test_id: &str, issue: u64) -> String {
        let record = serde_json::json!({
            "test_id": test_id,
            "matcher": {"digest": digest(TEXT)},
            "issue": issue,
            "class": "regression",
        });
        format!("intro\n```{FENCE_INFO}\n{record}\n```\n")
    }

    fn source(number: u64, status: &str, body: String) -> SignatureIssueSource {
        SignatureIssueSource {
            issue_id: Uuid::from_u128(u128::from(number)),
            display_number: number,
            status: status.to_string(),
            body,
        }
    }

    fn by_test(test_id: &str) -> AgentQueryFailureSignaturesRequestV1 {
        AgentQueryFailureSignaturesRequestV1 {
            test_id: Some(test_id.to_string()),
            digest: None,
        }
    }

    #[test]
    fn query_by_test_name_and_by_digest_returns_the_owner_issue() {
        let issues = [source(7, "Open", body("a::b", 7))];
        let named = AgentQueryFailureSignaturesResultV1::from_issues(&issues, &by_test("a::b"));
        assert_eq!(named.records.len(), 1);
        assert_eq!(named.records[0].record.issue, 7);
        assert_eq!(named.records[0].issue_id, Uuid::from_u128(7));
        assert_eq!(named.records[0].issue_status, "Open");
        let hashed = AgentQueryFailureSignaturesResultV1::from_issues(
            &issues,
            &AgentQueryFailureSignaturesRequestV1 {
                test_id: None,
                digest: Some(digest(TEXT)),
            },
        );
        assert_eq!(hashed.records, named.records);
    }

    #[test]
    fn closed_cancelled_owner_issue_is_expired_and_in_progress_is_live() {
        let issues = [
            source(1, "Closed", body("a::b", 1)),
            source(2, "Cancelled", body("a::b", 2)),
            source(3, "InProgress", body("a::b", 3)),
        ];
        let result = AgentQueryFailureSignaturesResultV1::from_issues(&issues, &by_test("a::b"));
        let owners: Vec<u64> = result.records.iter().map(|m| m.record.issue).collect();
        assert_eq!(owners, [3]);
    }

    #[test]
    fn unknown_query_is_a_typed_empty_result() {
        let issues = [source(7, "Open", body("a::b", 7))];
        let result =
            AgentQueryFailureSignaturesResultV1::from_issues(&issues, &by_test("no::such"));
        assert_eq!(result, AgentQueryFailureSignaturesResultV1::default());
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["records"], serde_json::json!([]));
        assert_eq!(json["malformed_issues"], serde_json::json!([]));
    }

    #[test]
    fn malformed_block_is_reported_not_fatal() {
        let bad = format!("```{FENCE_INFO}\n{{not json}}\n```\n");
        let issues = [
            source(4, "Open", bad),
            source(5, "Open", body("a::b", 5)),
            // A record naming a different Issue than its host is malformed.
            source(6, "Open", body("a::b", 99)),
        ];
        let result = AgentQueryFailureSignaturesResultV1::from_issues(&issues, &by_test("a::b"));
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.malformed_issues, [4, 6]);
    }

    #[test]
    fn request_validation_is_strict() {
        assert!(
            AgentQueryFailureSignaturesRequestV1::default()
                .validate()
                .is_err()
        );
        assert!(by_test("a::b").validate().is_ok());
        assert!(by_test("  ").validate().is_err());
        let bad_digest = AgentQueryFailureSignaturesRequestV1 {
            test_id: None,
            digest: Some("ABC".into()),
        };
        assert!(bad_digest.validate().is_err());
        assert!(
            serde_json::from_value::<AgentQueryFailureSignaturesRequestV1>(
                serde_json::json!({"test_id":"a","extra":1})
            )
            .is_err()
        );
    }
}
