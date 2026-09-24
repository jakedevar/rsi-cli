//! Pure-function containment validator + cycle detector for the hierarchical
//! session tree (Group/Epic organizational nodes + leaf sessions).
//!
//! This module is intentionally free of side effects: the RPC layer (Phase 2)
//! calls `validate_containment` and `detect_cycle` before persisting any
//! parent_id change. Neither fn touches the DB nor the filesystem — both take
//! only Copy inputs plus, for the cycle detector, a closure that returns the
//! current parent of a node.

use rsi_common::types::{SessionKind, legal_children};
use uuid::Uuid;

/// Every failure mode the hierarchy layer can surface to callers.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContainmentError {
    /// Attempted child kind is not allowed under this parent kind.
    /// `parent = None` means "being placed at root".
    #[error("Illegal parent: {parent:?} cannot contain {child:?}")]
    IllegalParent {
        parent: Option<SessionKind>,
        child: SessionKind,
    },

    /// Walking up from `proposed_parent` eventually reaches the session
    /// being reparented, which would form a cycle. The `chain` is the
    /// ancestor chain (starting at `proposed_parent`) up to the session.
    #[error("Cycle detected: {chain:?}")]
    CycleDetected { chain: Vec<Uuid> },
}

/// Check that `child_kind` is allowed inside `parent_kind`. `None` parent
/// means "being placed at root".
///
/// This is the canonical source of truth for the containment matrix:
/// - Root → [Standard, Group]
/// - Group → [Standard, Epic]
/// - Epic → [Story, Task, Bug]
/// - any leaf → nothing
///
/// Encoded via `rsi_common::legal_children`, so the matrix lives in exactly
/// one place.
pub fn validate_containment(
    parent_kind: Option<SessionKind>,
    child_kind: SessionKind,
) -> Result<(), ContainmentError> {
    let legal = legal_children(parent_kind);
    if legal.contains(&child_kind) {
        Ok(())
    } else {
        Err(ContainmentError::IllegalParent {
            parent: parent_kind,
            child: child_kind,
        })
    }
}

/// Walk upward from `proposed_parent` via `parent_of`; reject if we ever
/// encounter `session_id` (cycle). Also rejects the degenerate self-cycle
/// `session_id == proposed_parent`.
///
/// `parent_of(id)` returns the current parent_id of `id`, or `None` if `id`
/// is at the root (or unknown). The callee owns the lookup mechanism — this
/// fn is free of storage concerns.
///
/// Guarded against accidental infinite loops by a visited set; if an
/// unrelated pre-existing cycle is encountered in the ancestor chain, the
/// walk terminates cleanly rather than hanging.
pub fn detect_cycle(
    session_id: Uuid,
    proposed_parent: Uuid,
    parent_of: impl Fn(Uuid) -> Option<Uuid>,
) -> Result<(), ContainmentError> {
    if session_id == proposed_parent {
        return Err(ContainmentError::CycleDetected {
            chain: vec![session_id],
        });
    }

    let mut chain = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut cursor = Some(proposed_parent);

    while let Some(node) = cursor {
        if !visited.insert(node) {
            // Pre-existing cycle in the ancestor chain (not involving
            // `session_id`). Return what we've walked so far — the caller
            // treats any non-Ok result as a rejection.
            chain.push(node);
            return Err(ContainmentError::CycleDetected { chain });
        }
        chain.push(node);
        if node == session_id {
            return Err(ContainmentError::CycleDetected { chain });
        }
        cursor = parent_of(node);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const ALL_KINDS: &[SessionKind] = &[
        SessionKind::Standard,
        SessionKind::TaskRabbit,
        SessionKind::Bug,
        SessionKind::Group,
        SessionKind::Epic,
        SessionKind::Story,
        SessionKind::Task,
        SessionKind::Feature,
        SessionKind::Refactor,
        SessionKind::Research,
    ];

    #[test]
    fn containment_matrix_matches_legal_children_for_all_parents() {
        // Parent = None (root)
        let parents: Vec<Option<SessionKind>> = std::iter::once(None)
            .chain(ALL_KINDS.iter().copied().map(Some))
            .collect();

        for parent in parents {
            let legal = legal_children(parent);
            for &child in ALL_KINDS {
                let expected_ok = legal.contains(&child);
                let actual = validate_containment(parent, child);
                assert_eq!(
                    actual.is_ok(),
                    expected_ok,
                    "parent={:?} child={:?} expected_ok={} got={:?}",
                    parent,
                    child,
                    expected_ok,
                    actual
                );
            }
        }
    }

    #[test]
    fn root_allows_standard_and_group() {
        assert!(validate_containment(None, SessionKind::Standard).is_ok());
        assert!(validate_containment(None, SessionKind::Group).is_ok());
        assert!(validate_containment(None, SessionKind::Epic).is_err());
        assert!(validate_containment(None, SessionKind::Task).is_err());
        assert!(validate_containment(None, SessionKind::Story).is_err());
        assert!(validate_containment(None, SessionKind::Bug).is_err());
        assert!(validate_containment(None, SessionKind::TaskRabbit).is_err());
    }

    #[test]
    fn group_allows_standard_and_epic_only() {
        assert!(validate_containment(Some(SessionKind::Group), SessionKind::Standard).is_ok());
        assert!(validate_containment(Some(SessionKind::Group), SessionKind::Epic).is_ok());
        assert!(validate_containment(Some(SessionKind::Group), SessionKind::Group).is_err());
        assert!(validate_containment(Some(SessionKind::Group), SessionKind::Task).is_err());
    }

    #[test]
    fn epic_allows_story_task_bug() {
        assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Story).is_ok());
        assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Task).is_ok());
        assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Bug).is_ok());
        assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Standard).is_err());
        assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Group).is_err());
        assert!(validate_containment(Some(SessionKind::Epic), SessionKind::Epic).is_err());
    }

    #[test]
    fn leaf_kinds_cannot_contain_anything() {
        for &leaf in &[
            SessionKind::Standard,
            SessionKind::TaskRabbit,
            SessionKind::Bug,
            SessionKind::Story,
            SessionKind::Task,
            SessionKind::Feature,
            SessionKind::Refactor,
            SessionKind::Research,
        ] {
            for &child in ALL_KINDS {
                assert!(
                    validate_containment(Some(leaf), child).is_err(),
                    "leaf {:?} should reject child {:?}",
                    leaf,
                    child
                );
            }
        }
    }

    #[test]
    fn self_cycle_rejected() {
        let a = Uuid::new_v4();
        let r = detect_cycle(a, a, |_| None);
        assert!(matches!(r, Err(ContainmentError::CycleDetected { .. })));
    }

    #[test]
    fn two_hop_cycle_rejected() {
        // B -> A already. Moving A under B would create A -> B -> A.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let parents: HashMap<Uuid, Uuid> = [(b, a)].into_iter().collect();
        let r = detect_cycle(a, b, |id| parents.get(&id).copied());
        assert!(matches!(r, Err(ContainmentError::CycleDetected { .. })));
    }

    #[test]
    fn three_hop_cycle_rejected() {
        // C -> B -> A already. Moving A under C would create A -> C -> B -> A.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let parents: HashMap<Uuid, Uuid> = [(c, b), (b, a)].into_iter().collect();
        let r = detect_cycle(a, c, |id| parents.get(&id).copied());
        assert!(matches!(r, Err(ContainmentError::CycleDetected { .. })));
    }

    #[test]
    fn non_cycle_accepted() {
        // A and B are siblings; C has no parent. Putting A under C is fine.
        let a = Uuid::new_v4();
        let c = Uuid::new_v4();
        let parents: HashMap<Uuid, Uuid> = HashMap::new();
        let r = detect_cycle(a, c, |id| parents.get(&id).copied());
        assert!(r.is_ok());
    }

    /// Spawn gate guard: the same `is_leaf_kind` predicate `launch_session`
    /// uses to reject container kinds. Keeping the assertion here (rather
    /// than hitting a full SessionManager) exercises the same decision
    /// boundary without the test-harness overhead.
    #[test]
    fn spawn_gate_rejects_container_kinds() {
        use rsi_common::is_leaf_kind;
        // Leaf kinds — launch proceeds.
        assert!(is_leaf_kind(SessionKind::Standard));
        assert!(is_leaf_kind(SessionKind::TaskRabbit));
        assert!(is_leaf_kind(SessionKind::Bug));
        assert!(is_leaf_kind(SessionKind::Story));
        assert!(is_leaf_kind(SessionKind::Task));
        // Container kinds — launch_session returns InvalidParam.
        assert!(!is_leaf_kind(SessionKind::Group));
        assert!(!is_leaf_kind(SessionKind::Epic));
    }

    #[test]
    fn cycle_in_ancestors_does_not_hang() {
        // Pathological: D -> E -> D (pre-existing cycle not involving A).
        // Walk terminates; result is rejection.
        let a = Uuid::new_v4();
        let d = Uuid::new_v4();
        let e = Uuid::new_v4();
        let parents: HashMap<Uuid, Uuid> = [(d, e), (e, d)].into_iter().collect();
        let r = detect_cycle(a, d, |id| parents.get(&id).copied());
        assert!(matches!(r, Err(ContainmentError::CycleDetected { .. })));
    }
}
