//! One classification of the terminal custody shapes the startup publishers
//! write (#2050).
//!
//! The Session dependency-health index (V175 relevance classifier) and the
//! runtime settlement decoder (`cohort_settlement`) both have to decide whether
//! a Session whose projection is `historical_cleanup_failed`, `quarantined`,
//! `historical_transferred`, `historical_purged` or `invalid` is settled
//! history, history that still protects a retained root, an unproven row, or a
//! custody contradiction. They used to hold two copies of the predicates, and
//! the copies drifted: the released V120 helper reported every purged root that
//! retains its `root_missing` observation, and every ownerless quarantine,
//! as an unhealthy dependency while the decoder (correctly) treated both as
//! publisher output. Each caller now builds [`TerminalCustodyFacts`] from its
//! own row source and asks [`classify_terminal_custody_shape`], so a shape the
//! publishers write cannot be classified differently by the two consumers.
//!
//! The predicates are the decoder's: the publishers' invariants are
//! `publish_startup_terminal_root`, `publish_startup_rootless_*`,
//! `quarantine_startup_terminal_root` and `invalidate_startup_session` in
//! `sandbox_custody.rs`.

/// Every column either consumer reads about one Session, its execution
/// projection and the custody root the projection names. Text columns are
/// bounded UTF-8 already; generations are positive.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TerminalCustodyFacts<'a> {
    /// Projection exists with schema/projection version 1 and both timestamps.
    pub authenticated_projection: bool,
    pub session_id: Option<&'a str>,
    pub working_dir: Option<&'a str>,
    pub session_sandbox_root: Option<&'a str>,
    pub session_sandbox_branch: Option<&'a str>,
    pub session_custody_id: Option<&'a str>,
    pub sandbox_kind: Option<&'a str>,
    pub cleanup_state: Option<&'a str>,
    pub execution_state: Option<&'a str>,
    pub freshness: Option<&'a str>,
    pub effective_cwd: Option<&'a str>,
    pub projection_canonical_repo_dir: Option<&'a str>,
    pub projection_custody_id: Option<&'a str>,
    pub projection_generation: Option<u64>,
    pub projection_error_code: Option<&'a str>,
    pub root_custody_id: Option<&'a str>,
    pub root_state: Option<&'a str>,
    pub root_owner_session_id: Option<&'a str>,
    pub root_generation: Option<u64>,
    pub root_canonical_repo_dir: Option<&'a str>,
    pub root_sandbox_root: Option<&'a str>,
    pub root_sandbox_branch: Option<&'a str>,
    pub root_validation_state: Option<&'a str>,
    pub root_validated_generation: Option<u64>,
    pub root_validation_error_code: Option<&'a str>,
}

/// What a terminal projection state means for dependency decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalCustodyShape {
    /// Authenticated settled history: nothing can execute in, or be restored
    /// into, any path it names.
    SettledHistory,
    /// An ownerless quarantine whose Session paths were cleared. It cannot
    /// execute, but the retained root identity still protects its own path
    /// from other candidates.
    RetainedRootProtection,
    /// The row claims to be invalid history but is not the authenticated
    /// path-free shape. It may still name paths and stays under scrutiny.
    Unproven,
    /// A publisher invariant is broken for this state: a custody
    /// contradiction. The reason is the settlement diagnostic.
    Contradiction(&'static str),
}

impl<'a> TerminalCustodyFacts<'a> {
    /// Build facts from the settlement decoder's 30 guarded columns, in the
    /// order of its `SELECT` (`cohort_settlement::source_worktree_inventory`).
    pub(crate) fn from_settlement_fields(fields: &'a [Option<String>]) -> Self {
        let text = move |index: usize| fields[index].as_deref();
        let generation = move |index: usize| {
            fields[index]
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
        };
        Self {
            authenticated_projection: fields[26].is_some()
                && fields[27].as_deref() == Some("1")
                && fields[28].as_deref() == Some("1")
                && fields[29].is_some(),
            session_id: text(0),
            working_dir: text(2),
            session_sandbox_root: text(3),
            effective_cwd: text(4),
            cleanup_state: text(5),
            execution_state: text(6),
            freshness: text(7),
            projection_custody_id: text(8),
            projection_generation: generation(9),
            root_state: text(10),
            root_owner_session_id: text(11),
            root_generation: generation(12),
            projection_error_code: text(13),
            sandbox_kind: text(15),
            session_sandbox_branch: text(16),
            session_custody_id: text(17),
            projection_canonical_repo_dir: text(18),
            root_custody_id: text(19),
            root_canonical_repo_dir: text(20),
            root_sandbox_root: text(21),
            root_sandbox_branch: text(22),
            root_validation_state: text(23),
            root_validated_generation: generation(24),
            root_validation_error_code: text(25),
        }
    }

    /// The projection names no custody root at all.
    fn rootless_projection(&self) -> bool {
        self.root_state.is_none()
            && self.root_owner_session_id.is_none()
            && self.root_generation.is_none()
            && self.root_custody_id.is_none()
            && self.root_canonical_repo_dir.is_none()
            && self.root_sandbox_root.is_none()
            && self.root_sandbox_branch.is_none()
            && self.root_validation_state.is_none()
            && self.root_validated_generation.is_none()
            && self.root_validation_error_code.is_none()
    }

    /// Session, projection and root name one custody at one generation.
    fn linked_to_root_generation(&self) -> bool {
        self.session_custody_id == self.projection_custody_id
            && self.projection_custody_id == self.root_custody_id
            && self.projection_generation == self.root_generation
            && self.projection_generation == self.root_validated_generation
    }
}

/// Classify a terminal projection state. `None` means the execution state is
/// not one of the five terminal states (`ordinary_unsandboxed`,
/// `live_sandboxed`, absent or unknown); those are the caller's decision.
pub(crate) fn classify_terminal_custody_shape(
    facts: &TerminalCustodyFacts<'_>,
) -> Option<TerminalCustodyShape> {
    use TerminalCustodyShape::{Contradiction, RetainedRootProtection, SettledHistory, Unproven};
    let f = facts;
    Some(match f.execution_state? {
        "historical_cleanup_failed" => {
            let common = f.authenticated_projection
                && f.effective_cwd.is_none()
                && f.cleanup_state == Some("Failed")
                && f.freshness == Some("verified")
                && f.projection_error_code.is_none()
                && f.sandbox_kind == Some("GitWorktree")
                && f.projection_canonical_repo_dir == f.working_dir;
            let rootless = common
                && f.session_sandbox_root.is_none()
                && f.session_sandbox_branch.is_none()
                && f.session_custody_id.is_none()
                && f.projection_custody_id.is_none()
                && f.projection_generation.is_none()
                && f.rootless_projection();
            let linked_failed = common
                && f.session_sandbox_root == f.root_sandbox_root
                && f.session_sandbox_branch == f.root_sandbox_branch
                && f.linked_to_root_generation()
                && f.working_dir == f.root_canonical_repo_dir
                && f.projection_canonical_repo_dir == f.root_canonical_repo_dir
                && f.root_state == Some("failed")
                && f.root_owner_session_id.is_none()
                && f.root_validation_state == Some("verified")
                && f.root_validation_error_code.is_none();
            if rootless || linked_failed {
                SettledHistory
            } else {
                Contradiction("historical_cleanup_failed_custody_mismatch")
            }
        }
        "quarantined" => {
            let session_paths_cleared =
                f.session_sandbox_root.is_none() && f.session_sandbox_branch.is_none();
            let path_identity_matches = (f.session_sandbox_root == f.root_sandbox_root
                && f.session_sandbox_branch == f.root_sandbox_branch)
                || session_paths_cleared;
            if !f.authenticated_projection
                || f.sandbox_kind != Some("GitWorktree")
                || f.effective_cwd.is_some()
                || f.cleanup_state != Some("Failed")
                || f.freshness != Some("invalid")
                || !path_identity_matches
                || !f.linked_to_root_generation()
                || f.working_dir != f.root_canonical_repo_dir
                || f.projection_canonical_repo_dir != f.root_canonical_repo_dir
                || f.root_state != Some("quarantined")
                || f.root_owner_session_id.is_some()
                || f.root_validation_state != Some("invalid")
                || f.projection_error_code.is_none()
                || f.projection_error_code != f.root_validation_error_code
            {
                Contradiction("quarantined_custody_mismatch")
            } else if session_paths_cleared {
                // Startup can publish an ownerless quarantine after the Session
                // paths were cleared. Its retained root identity still needs
                // protection, but cannot poison unrelated roots.
                RetainedRootProtection
            } else {
                SettledHistory
            }
        }
        "historical_transferred" => {
            let transferred_forward = matches!(
                (f.projection_generation, f.root_generation),
                (Some(projection), Some(root)) if projection < root
            );
            if !f.authenticated_projection
                || f.freshness != Some("verified")
                || f.effective_cwd.is_some()
                || f.projection_error_code.is_some()
                || f.sandbox_kind != Some("GitWorktree")
                || f.cleanup_state != Some("Live")
                || f.session_sandbox_root != f.root_sandbox_root
                || f.session_sandbox_branch != f.root_sandbox_branch
                || f.session_custody_id != f.projection_custody_id
                || f.projection_custody_id != f.root_custody_id
                || f.working_dir != f.root_canonical_repo_dir
                || f.projection_canonical_repo_dir != f.root_canonical_repo_dir
                || f.root_state != Some("live")
                || f.root_owner_session_id.is_none()
                || f.root_owner_session_id == f.session_id
                || f.root_validation_state != Some("verified")
                || f.root_validated_generation != f.root_generation
                || f.root_validation_error_code.is_some()
                || !transferred_forward
            {
                Contradiction("historical_transferred_custody_mismatch")
            } else {
                SettledHistory
            }
        }
        "historical_purged" => {
            let common = f.authenticated_projection
                && f.freshness == Some("verified")
                && f.effective_cwd.is_none()
                && f.projection_error_code.is_none()
                && f.projection_canonical_repo_dir == f.working_dir;
            let rootless = common
                && f.session_sandbox_root.is_none()
                && f.session_sandbox_branch.is_none()
                && f.session_custody_id.is_none()
                && f.projection_custody_id.is_none()
                && f.projection_generation.is_none()
                && matches!(
                    (f.sandbox_kind, f.cleanup_state),
                    (None, None) | (Some("GitWorktree"), Some("Purged"))
                )
                && f.rootless_projection();
            // Startup authenticates the terminal projection while retaining the
            // root's last filesystem observation. A missing purged root is the
            // expected `invalid/root_missing` observation, not conflicting
            // custody (#2050).
            let linked = common
                && f.sandbox_kind == Some("GitWorktree")
                && f.cleanup_state == Some("Purged")
                && f.session_sandbox_root.is_none()
                && f.session_sandbox_branch.is_none()
                && f.linked_to_root_generation()
                && f.working_dir == f.root_canonical_repo_dir
                && f.root_state == Some("purged")
                && f.root_owner_session_id.is_none()
                && matches!(
                    (f.root_validation_state, f.root_validation_error_code),
                    (Some("verified"), None) | (Some("invalid"), Some("root_missing"))
                );
            if rootless || linked {
                SettledHistory
            } else {
                Contradiction("historical_purged_custody_mismatch")
            }
        }
        "invalid" => {
            let inert = f.authenticated_projection
                && f.freshness == Some("invalid")
                && f.effective_cwd.is_none()
                && f.projection_error_code.is_some()
                && f.session_sandbox_root.is_none()
                && f.session_sandbox_branch.is_none()
                && f.session_custody_id.is_none()
                && f.projection_custody_id.is_none()
                && f.projection_generation.is_none()
                && f.projection_canonical_repo_dir == f.working_dir
                && f.rootless_projection();
            if inert { SettledHistory } else { Unproven }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::TerminalCustodyShape::{
        Contradiction, RetainedRootProtection, SettledHistory, Unproven,
    };
    use super::*;

    const SESSION: &str = "00000000-0000-0000-0000-0000000000a1";
    const OWNER: &str = "00000000-0000-0000-0000-0000000000a2";
    const CUSTODY: &str = "00000000-0000-0000-0000-0000000000c1";
    const REPO: &str = "/repo";
    const SANDBOX: &str = "/sandboxes/a1";
    const BRANCH: &str = "rsi/a1";

    fn rootless_history(state: &'static str) -> TerminalCustodyFacts<'static> {
        TerminalCustodyFacts {
            authenticated_projection: true,
            session_id: Some(SESSION),
            working_dir: Some(REPO),
            projection_canonical_repo_dir: Some(REPO),
            execution_state: Some(state),
            freshness: Some("verified"),
            ..TerminalCustodyFacts::default()
        }
    }

    fn linked(state: &'static str, root_state: &'static str) -> TerminalCustodyFacts<'static> {
        TerminalCustodyFacts {
            sandbox_kind: Some("GitWorktree"),
            session_custody_id: Some(CUSTODY),
            projection_custody_id: Some(CUSTODY),
            projection_generation: Some(2),
            root_custody_id: Some(CUSTODY),
            root_state: Some(root_state),
            root_generation: Some(2),
            root_validated_generation: Some(2),
            root_canonical_repo_dir: Some(REPO),
            root_sandbox_root: Some(SANDBOX),
            root_sandbox_branch: Some(BRANCH),
            root_validation_state: Some("verified"),
            ..rootless_history(state)
        }
    }

    fn purged_root(
        validation: &'static str,
        error: Option<&'static str>,
    ) -> TerminalCustodyFacts<'static> {
        TerminalCustodyFacts {
            cleanup_state: Some("Purged"),
            root_validation_state: Some(validation),
            root_validation_error_code: error,
            ..linked("historical_purged", "purged")
        }
    }

    fn cleanup_failed_root() -> TerminalCustodyFacts<'static> {
        TerminalCustodyFacts {
            cleanup_state: Some("Failed"),
            session_sandbox_root: Some(SANDBOX),
            session_sandbox_branch: Some(BRANCH),
            ..linked("historical_cleanup_failed", "failed")
        }
    }

    fn quarantine(session_paths_cleared: bool) -> TerminalCustodyFacts<'static> {
        TerminalCustodyFacts {
            cleanup_state: Some("Failed"),
            freshness: Some("invalid"),
            session_sandbox_root: (!session_paths_cleared).then_some(SANDBOX),
            session_sandbox_branch: (!session_paths_cleared).then_some(BRANCH),
            projection_error_code: Some("ownership_conflict"),
            root_validation_state: Some("invalid"),
            root_validation_error_code: Some("ownership_conflict"),
            ..linked("quarantined", "quarantined")
        }
    }

    fn transferred() -> TerminalCustodyFacts<'static> {
        TerminalCustodyFacts {
            cleanup_state: Some("Live"),
            session_sandbox_root: Some(SANDBOX),
            session_sandbox_branch: Some(BRANCH),
            projection_generation: Some(1),
            root_owner_session_id: Some(OWNER),
            ..linked("historical_transferred", "live")
        }
    }

    fn classify(facts: TerminalCustodyFacts<'_>) -> Option<TerminalCustodyShape> {
        classify_terminal_custody_shape(&facts)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn publisher_shapes_are_settled_history_or_protect_their_retained_root() {
        // publish_startup_rootless_purged / _failed, with and without sandbox kind.
        assert_eq!(
            classify(rootless_history("historical_purged")),
            Some(SettledHistory)
        );
        assert_eq!(
            classify(TerminalCustodyFacts {
                sandbox_kind: Some("GitWorktree"),
                cleanup_state: Some("Purged"),
                ..rootless_history("historical_purged")
            }),
            Some(SettledHistory)
        );
        assert_eq!(
            classify(TerminalCustodyFacts {
                sandbox_kind: Some("GitWorktree"),
                cleanup_state: Some("Failed"),
                ..rootless_history("historical_cleanup_failed")
            }),
            Some(SettledHistory)
        );
        // publish_startup_terminal_root for a purged root, both observations a
        // startup pass can leave on it: verified, and the retained root_missing.
        assert_eq!(
            classify(purged_root("verified", None)),
            Some(SettledHistory)
        );
        assert_eq!(
            classify(purged_root("invalid", Some("root_missing"))),
            Some(SettledHistory),
            "a purged root retaining root_missing is publisher output, not custody conflict"
        );
        // publish_startup_terminal_root for a failed root.
        assert_eq!(classify(cleanup_failed_root()), Some(SettledHistory));
        // publish_startup_terminal_root for a quarantine: intact Session paths
        // are settled; cleared paths keep protecting the retained root.
        assert_eq!(classify(quarantine(false)), Some(SettledHistory));
        assert_eq!(classify(quarantine(true)), Some(RetainedRootProtection));
        // A transferred predecessor's historical projection.
        assert_eq!(classify(transferred()), Some(SettledHistory));
        // invalidate_startup_session on a rootless Session.
        assert_eq!(
            classify(TerminalCustodyFacts {
                freshness: Some("invalid"),
                projection_error_code: Some("worktree_mismatch"),
                ..rootless_history("invalid")
            }),
            Some(SettledHistory)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn custody_contradictions_are_never_settled_history() {
        let mismatch = |facts: TerminalCustodyFacts<'static>, reason: &'static str| {
            assert_eq!(classify(facts), Some(Contradiction(reason)), "{facts:?}");
        };
        // Purged: another validation error, a drifted generation, a live owner,
        // a path the Session still claims, an unauthenticated projection.
        let purged = "historical_purged_custody_mismatch";
        mismatch(purged_root("invalid", Some("ownership_conflict")), purged);
        mismatch(purged_root("verified", Some("root_missing")), purged);
        mismatch(
            TerminalCustodyFacts {
                projection_generation: Some(3),
                ..purged_root("invalid", Some("root_missing"))
            },
            purged,
        );
        mismatch(
            TerminalCustodyFacts {
                root_owner_session_id: Some(OWNER),
                ..purged_root("verified", None)
            },
            purged,
        );
        mismatch(
            TerminalCustodyFacts {
                session_sandbox_root: Some(SANDBOX),
                ..purged_root("verified", None)
            },
            purged,
        );
        mismatch(
            TerminalCustodyFacts {
                authenticated_projection: false,
                ..purged_root("verified", None)
            },
            purged,
        );
        mismatch(
            TerminalCustodyFacts {
                root_state: Some("live"),
                ..purged_root("verified", None)
            },
            purged,
        );
        // Quarantine: generation drift, a half-cleared Session path, a missing
        // or different error code, an owner.
        let quarantined = "quarantined_custody_mismatch";
        mismatch(
            TerminalCustodyFacts {
                projection_generation: Some(3),
                ..quarantine(true)
            },
            quarantined,
        );
        mismatch(
            TerminalCustodyFacts {
                session_sandbox_branch: None,
                ..quarantine(false)
            },
            quarantined,
        );
        mismatch(
            TerminalCustodyFacts {
                projection_error_code: Some("root_missing"),
                ..quarantine(true)
            },
            quarantined,
        );
        mismatch(
            TerminalCustodyFacts {
                root_owner_session_id: Some(OWNER),
                ..quarantine(false)
            },
            quarantined,
        );
        // Cleanup failed and transferred.
        mismatch(
            TerminalCustodyFacts {
                root_state: Some("quarantined"),
                ..cleanup_failed_root()
            },
            "historical_cleanup_failed_custody_mismatch",
        );
        let transferred_reason = "historical_transferred_custody_mismatch";
        mismatch(
            TerminalCustodyFacts {
                root_owner_session_id: Some(SESSION),
                ..transferred()
            },
            transferred_reason,
        );
        mismatch(
            TerminalCustodyFacts {
                projection_generation: Some(2),
                ..transferred()
            },
            transferred_reason,
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn invalid_projection_with_a_named_custody_is_unproven_and_other_states_are_the_callers() {
        assert_eq!(
            classify(TerminalCustodyFacts {
                freshness: Some("invalid"),
                projection_error_code: Some("ownership_conflict"),
                projection_custody_id: Some(CUSTODY),
                projection_generation: Some(3),
                ..rootless_history("invalid")
            }),
            Some(Unproven)
        );
        for state in ["ordinary_unsandboxed", "live_sandboxed", "bogus"] {
            assert_eq!(classify(rootless_history(state)), None, "{state}");
        }
        assert_eq!(
            classify(TerminalCustodyFacts::default()),
            None,
            "an absent execution state is not a terminal shape"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn settlement_fields_map_every_column_to_its_fact() {
        let text = |value: &str| Some(value.to_owned());
        let fields: Vec<Option<String>> = vec![
            text(SESSION),                          // 0 id
            text("Archived"),                       // 1 status
            text(REPO),                             // 2 working_dir
            None,                                   // 3 sandbox_root
            None,                                   // 4 effective_cwd
            text("Failed"),                         // 5 cleanup
            text("quarantined"),                    // 6 execution_state
            text("invalid"),                        // 7 freshness
            text(CUSTODY),                          // 8 projection custody
            text("2"),                              // 9 projection generation
            text("quarantined"),                    // 10 root state
            None,                                   // 11 root owner
            text("2"),                              // 12 root generation
            text("ownership_conflict"),             // 13 projection error
            text("Standard"),                       // 14 kind
            text("GitWorktree"),                    // 15 sandbox kind
            None,                                   // 16 branch
            text(CUSTODY),                          // 17 session custody
            text(REPO),                             // 18 projection repo
            text(CUSTODY),                          // 19 root custody
            text(REPO),                             // 20 root repo
            text(SANDBOX),                          // 21 root sandbox
            text(BRANCH),                           // 22 root branch
            text("invalid"),                        // 23 root validation
            text("2"),                              // 24 validated generation
            text("ownership_conflict"),             // 25 root error
            text("2099-01-01T00:00:00.000000000Z"), // 26 validated_at
            text("1"),                              // 27 schema
            text("1"),                              // 28 projection version
            text("2099-01-01T00:00:00.000000000Z"), // 29 updated_at
        ];
        let facts = TerminalCustodyFacts::from_settlement_fields(&fields);
        assert_eq!(
            classify_terminal_custody_shape(&facts),
            Some(RetainedRootProtection)
        );
        assert_eq!(facts.session_id, Some(SESSION));
        assert_eq!(facts.root_sandbox_root, Some(SANDBOX));
        assert_eq!(facts.projection_generation, Some(2));
        let mut unauthenticated = fields.clone();
        unauthenticated[27] = text("2");
        let facts = TerminalCustodyFacts::from_settlement_fields(&unauthenticated);
        assert!(!facts.authenticated_projection);
        assert_eq!(
            classify_terminal_custody_shape(&facts),
            Some(Contradiction("quarantined_custody_mismatch"))
        );
    }
}
