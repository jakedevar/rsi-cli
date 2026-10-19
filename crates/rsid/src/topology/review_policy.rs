//! The review policy an Issue-pipeline execution lands under (#2304 slice 3).
//!
//! AGENTS.md reserves a pre-merge review for schema migrations and
//! credential, IAM or network-exposure changes, and wants one post-land review
//! of other authority, custody and security changes. The manager picks the
//! route when it starts the execution, as the reserved `inputs.review_policy`
//! (`ordinary`, `pre_merge`, `post_land`); this module is the daemon's check
//! of that choice:
//!
//! * at execute admission the chosen value must be one the graph's review
//!   structure covers ([`resolve`]), and it is recorded with the input;
//! * at enqueue the candidate commit is classified ([`Candidate`]) and a
//!   source that needs a stricter review than the graph provides is blocked
//!   with the required policy and exact source SHA ([`block_reason`]). The
//!   daemon never rewrites an accepted definition and never grants a launch:
//!   the owning manager starts the right execution.
//!
//! The path table is a conservative backstop, not proof that a source has no
//! security implications: a worker's claim that no review is needed is never
//! read. Only the policy-bearing Issue-pipeline starters (or an execution that
//! passes the reserved input) are checked; every other topology keeps its
//! behaviour.

use std::collections::HashSet;

use rsi_common::types::TopologyStep;
use rsi_graph::format::WorkflowDefinition;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::Result;
use crate::topology::steps::WorkflowSteps;

/// The reserved execution input naming the review policy.
pub(crate) const REVIEW_POLICY_INPUT_KEY: &str = "review_policy";

/// How much review a landing gets. Ordered from least to most strict: a graph
/// that covers a policy also covers every earlier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReviewPolicy {
    /// Verification only; no reviewer on the path.
    Ordinary,
    /// A review of what landed follows publication.
    PostLand,
    /// An accepted review of the exact head precedes the enqueue.
    PreMerge,
}

impl ReviewPolicy {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::PostLand => "post_land",
            Self::PreMerge => "pre_merge",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "ordinary" => Some(Self::Ordinary),
            "post_land" => Some(Self::PostLand),
            "pre_merge" => Some(Self::PreMerge),
            _ => None,
        }
    }
}

// ─── the graph's coverage ──────────────────────────────────────────────────

/// The policy the land node `node` is covered by: `PreMerge` when it lands a
/// reviewed commit, `PostLand` when a review of its accepted node follows it
/// (a `review_required` route to a pre-land review does not count, #2688),
/// else `Ordinary`. `None` for a node that is not a land node.
pub(crate) fn land_coverage(
    workflow: &WorkflowDefinition,
    steps: &WorkflowSteps,
    node: &str,
) -> Option<ReviewPolicy> {
    let Some(TopologyStep::Land { accepted, .. }) = steps.step(node) else {
        return None;
    };
    if steps.is_review(accepted) {
        return Some(ReviewPolicy::PreMerge);
    }
    // Breadth-first over the forward edges: a review of the accepted node
    // downstream of the land is the post-land review of that commit.
    let mut seen: HashSet<&str> = HashSet::from([node]);
    let mut queue = vec![node];
    while let Some(current) = queue.pop() {
        for edge in workflow.edges.iter().filter(|edge| edge.source == current) {
            let next = edge.target.as_str();
            // #2688: the pre-land review a refused land routes to is not a
            // review of what landed; it never makes the land `post_land`.
            if steps.when(current, next) == rsi_common::types::EdgeWhen::ReviewRequired
                || !seen.insert(next)
            {
                continue;
            }
            if steps.review_of(next) == Some(accepted.as_str()) {
                return Some(ReviewPolicy::PostLand);
            }
            queue.push(next);
        }
    }
    Some(ReviewPolicy::Ordinary)
}

/// The weakest policy any land node of the graph is covered by (`Ordinary`
/// when it has none).
pub(crate) fn graph_coverage(workflow: &WorkflowDefinition, steps: &WorkflowSteps) -> ReviewPolicy {
    workflow
        .nodes
        .iter()
        .filter_map(|node| land_coverage(workflow, steps, &node.id))
        .min()
        .unwrap_or(ReviewPolicy::Ordinary)
}

// ─── the execution's recorded choice ───────────────────────────────────────

/// Topologies that carry a review policy by identity: the three Issue-pipeline
/// starters.
fn policy_starter(topology_id: Option<Uuid>) -> bool {
    let Some(id) = topology_id else {
        return false;
    };
    crate::topology::starters::STARTERS.iter().any(|starter| {
        starter.id == id
            && [
                crate::topology::starters::ISSUE_PIPELINE,
                crate::topology::starters::ISSUE_PIPELINE_REVIEWED,
                crate::topology::starters::ISSUE_PIPELINE_POST_REVIEW,
            ]
            .contains(&starter.name)
    })
}

/// The policy an execution of `workflow` is governed by, or `None` when the
/// execution is outside the policy-bearing starters and passed no
/// `inputs.review_policy` (behaviour unchanged). A policy-bearing execution
/// that passed no value is governed by what its graph covers.
///
/// # Errors
/// The typed refusals `review_policy_invalid` (not one of the three values)
/// and `review_policy_graph_mismatch` (the graph lacks the review structure
/// the value requires).
pub(crate) fn resolve(
    topology_id: Option<Uuid>,
    workflow: &WorkflowDefinition,
    inputs: &Value,
) -> Result<Option<ReviewPolicy>> {
    let chosen = match inputs.get(REVIEW_POLICY_INPUT_KEY) {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(ReviewPolicy::parse)
                .ok_or_else(|| {
                    crate::topology::agent::refusal(
                        "review_policy_invalid",
                        "inputs.review_policy must be one of ordinary, pre_merge or post_land",
                    )
                })?,
        ),
    };
    if chosen.is_none() && !policy_starter(topology_id) {
        return Ok(None);
    }
    let steps =
        WorkflowSteps::from_workflow(workflow).map_err(crate::error::DaemonError::InvalidParam)?;
    let coverage = graph_coverage(workflow, &steps);
    let policy = chosen.unwrap_or(coverage);
    if policy > coverage {
        return Err(crate::topology::agent::refusal(
            "review_policy_graph_mismatch",
            &format!(
                "this topology's review structure covers {} but inputs.review_policy asks for {}: choose issue-pipeline-reviewed for pre_merge or issue-pipeline-post-review for post_land",
                coverage.as_str(),
                policy.as_str()
            ),
        ));
    }
    Ok(Some(policy))
}

/// Record the resolved policy with the execution's input snapshot.
pub(crate) fn record(inputs: &mut Value, policy: ReviewPolicy) {
    match inputs {
        Value::Object(object) => {
            object.insert(REVIEW_POLICY_INPUT_KEY.into(), policy.as_str().into());
        }
        Value::Null => *inputs = json!({ REVIEW_POLICY_INPUT_KEY: policy.as_str() }),
        _ => {}
    }
}

/// What the enqueue of the land node `node` must check, re-derived from the
/// execution's stored definition and recorded input: the node's coverage when
/// the execution is policy-bearing, `None` when it is not.
///
/// # Errors
/// A reason string when the recorded policy is invalid or no longer covered by
/// the graph.
pub(crate) fn enforcement(
    topology_id: Option<Uuid>,
    workflow: &WorkflowDefinition,
    input: Option<&Value>,
    node: &str,
) -> std::result::Result<Option<ReviewPolicy>, String> {
    let inputs = input.unwrap_or(&Value::Null);
    if resolve(topology_id, workflow, inputs)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Ok(None);
    }
    let steps = WorkflowSteps::from_workflow(workflow)?;
    Ok(land_coverage(workflow, &steps, node))
}

// ─── the candidate's requirement ───────────────────────────────────────────

/// What inspecting a landing candidate found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Candidate {
    /// Provably no classified change.
    Ordinary,
    /// A change that needs `policy`; `why` names the class.
    Requires {
        policy: ReviewPolicy,
        why: &'static str,
    },
    /// Git could not show what the source changes: fail closed.
    Unknown(&'static str),
}

enum Match {
    Prefix(&'static str),
    Exact(&'static str),
    /// A Rust source file whose path contains the fragment.
    RustFileContaining(&'static str),
}

struct PathRule {
    policy: ReviewPolicy,
    why: &'static str,
    matcher: Match,
}

impl PathRule {
    fn matches(&self, path: &str) -> bool {
        match self.matcher {
            Match::Prefix(prefix) => path.starts_with(prefix),
            Match::Exact(exact) => path == exact,
            Match::RustFileContaining(fragment) => path.ends_with(".rs") && path.contains(fragment),
        }
    }
}

/// Repository paths whose changes need a review beyond verification. Bounded:
/// a new class is a reviewed change to this table and its fixtures.
const PATH_RULES: &[PathRule] = &[
    // Pre-merge: schema migrations (numbered and provisional) and their pin.
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes a schema migration",
        matcher: Match::Prefix("crates/rsid-store/src/store/migrations/"),
    },
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes a schema migration declaration",
        matcher: Match::Prefix("tools/provisional-migrations/"),
    },
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes the released-migration pin",
        matcher: Match::Exact("tools/released-migrations.json"),
    },
    // Pre-merge: credentials and cloud identity.
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes the credential vault",
        matcher: Match::Prefix("crates/rsid-store/src/vault/"),
    },
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes provider credential handling",
        matcher: Match::RustFileContaining("provider_credential"),
    },
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes Bedrock IAM and credential setup",
        matcher: Match::Prefix("crates/rsid-store/src/bedrock"),
    },
    // Pre-merge: network exposure.
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes the remote-control network surface",
        matcher: Match::RustFileContaining("remote_control"),
    },
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes the satellite network link",
        matcher: Match::Prefix("crates/rsid/src/satellite/link"),
    },
    PathRule {
        policy: ReviewPolicy::PreMerge,
        why: "changes the daemon's socket and service exposure",
        matcher: Match::Prefix("scripts/systemd/"),
    },
    // Post-land: authority, custody and security guards.
    PathRule {
        policy: ReviewPolicy::PostLand,
        why: "changes agent or manager authority",
        matcher: Match::RustFileContaining("authority"),
    },
    PathRule {
        policy: ReviewPolicy::PostLand,
        why: "changes sandbox or session custody",
        matcher: Match::RustFileContaining("custody"),
    },
    PathRule {
        policy: ReviewPolicy::PostLand,
        why: "changes sandbox storage",
        matcher: Match::Prefix("crates/rsid-store/src/sandbox/"),
    },
    PathRule {
        policy: ReviewPolicy::PostLand,
        why: "changes path-safety or process control",
        matcher: Match::Prefix("crates/rsid-core/src/path_safety"),
    },
    PathRule {
        policy: ReviewPolicy::PostLand,
        why: "changes path-safety or process control",
        matcher: Match::Prefix("crates/rsid-core/src/process_control"),
    },
    PathRule {
        policy: ReviewPolicy::PostLand,
        why: "changes the git and agent hooks",
        matcher: Match::Prefix("tools/git-hooks/"),
    },
    PathRule {
        policy: ReviewPolicy::PostLand,
        why: "changes the agent hooks",
        matcher: Match::Prefix(".claude/hooks/"),
    },
    PathRule {
        policy: ReviewPolicy::PostLand,
        why: "changes the landing policy",
        matcher: Match::Prefix("crates/rsid/src/bin/rsi-rolling-land/"),
    },
];

/// Classify a candidate's changed paths against [`PATH_RULES`]; the strictest
/// matching rule wins.
pub(crate) fn classify_paths(paths: &[String]) -> Candidate {
    PATH_RULES
        .iter()
        .filter(|rule| paths.iter().any(|path| rule.matches(path)))
        .max_by_key(|rule| rule.policy)
        .map_or(Candidate::Ordinary, |rule| Candidate::Requires {
            policy: rule.policy,
            why: rule.why,
        })
}

/// Why a landing must not be enqueued: the candidate needs a stricter review
/// than `coverage`. `None`: the graph's review covers it. An unknown
/// inspection needs the pre-merge route, because the fix is a reviewer who can
/// read the source, never a guess.
pub(crate) fn block_reason(
    coverage: ReviewPolicy,
    source_commit: &str,
    candidate: &Candidate,
) -> Option<String> {
    let (required, what) = match candidate {
        Candidate::Ordinary => return None,
        Candidate::Requires { policy, why } => (*policy, format!("the source {why}")),
        Candidate::Unknown(why) => (ReviewPolicy::PreMerge, (*why).to_owned()),
    };
    (coverage < required).then(|| {
        format!(
            "{what}, so source {source_commit} needs the {} review policy but this execution's graph covers {}; nothing was enqueued. Start the {} execution (issue-pipeline-reviewed for pre_merge, issue-pipeline-post-review for post_land) for this Issue with supersedes_execution",
            required.as_str(),
            coverage.as_str(),
            required.as_str(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|path| (*path).to_owned()).collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn every_path_class_has_a_fixture_and_the_strictest_wins() {
        let pre_merge = [
            "crates/rsid-store/src/store/migrations/v901.rs",
            "tools/provisional-migrations/x.json",
            "tools/released-migrations.json",
            "crates/rsid-store/src/vault/secret.rs",
            "crates/rsi/src/provider_credential_rows.rs",
            "crates/rsid-store/src/bedrock_setup.rs",
            "crates/rsid/src/remote_control.rs",
            "crates/rsid/src/satellite/link.rs",
            "scripts/systemd/rsid.socket",
        ];
        for path in pre_merge {
            assert!(
                matches!(
                    classify_paths(&paths(&[path])),
                    Candidate::Requires {
                        policy: ReviewPolicy::PreMerge,
                        ..
                    }
                ),
                "{path}"
            );
        }
        let post_land = [
            "crates/rsid/src/session/agent_authority.rs",
            "crates/rsid/src/session/branch_custody.rs",
            "crates/rsid-store/src/sandbox/custody.rs",
            "crates/rsid-core/src/path_safety.rs",
            "crates/rsid-core/src/process_control/unix.rs",
            "tools/git-hooks/pre-push",
            ".claude/hooks/stop_notify.js",
            "crates/rsid/src/bin/rsi-rolling-land/landing_policy.rs",
        ];
        for path in post_land {
            assert!(
                matches!(
                    classify_paths(&paths(&[path])),
                    Candidate::Requires {
                        policy: ReviewPolicy::PostLand,
                        ..
                    }
                ),
                "{path}"
            );
        }
        // A mixed change takes the strictest class.
        assert!(matches!(
            classify_paths(&paths(&[
                "crates/rsid/src/session/agent_authority.rs",
                "crates/rsid-store/src/vault/mod.rs",
            ])),
            Candidate::Requires {
                policy: ReviewPolicy::PreMerge,
                ..
            }
        ));
        // Ordinary tooling and documentation need no reviewer.
        assert_eq!(
            classify_paths(&paths(&[
                "scripts/fmt-changed.sh",
                "docs/agents/reference.md",
                "crates/rsid/src/topology/land.rs",
            ])),
            Candidate::Ordinary
        );
        assert_eq!(classify_paths(&[]), Candidate::Ordinary);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn block_reason_names_the_policy_and_the_exact_source() {
        let sha = "a".repeat(40);
        let vault = classify_paths(&paths(&["crates/rsid-store/src/vault/mod.rs"]));
        let reason = block_reason(ReviewPolicy::PostLand, &sha, &vault).unwrap();
        assert!(
            reason.contains("pre_merge") && reason.contains(&sha),
            "{reason}"
        );
        assert_eq!(block_reason(ReviewPolicy::PreMerge, &sha, &vault), None);
        assert_eq!(
            block_reason(ReviewPolicy::Ordinary, &sha, &Candidate::Ordinary),
            None
        );
        // An unknown inspection needs the pre-merge route and fails closed.
        let unknown = Candidate::Unknown("git could not list the source's changed paths");
        assert!(block_reason(ReviewPolicy::PostLand, &sha, &unknown).is_some());
        assert_eq!(block_reason(ReviewPolicy::PreMerge, &sha, &unknown), None);
        // A post-land graph covers a post-land class but not an ordinary graph.
        let custody = classify_paths(&paths(&["crates/rsid/src/session/branch_custody.rs"]));
        assert_eq!(block_reason(ReviewPolicy::PostLand, &sha, &custody), None);
        let reason = block_reason(ReviewPolicy::Ordinary, &sha, &custody).unwrap();
        assert!(
            reason.contains("post_land") && reason.contains(&sha),
            "{reason}"
        );
    }
}
