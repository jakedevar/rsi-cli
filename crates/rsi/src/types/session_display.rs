//! Canonical TUI display identity for sessions.

use std::collections::{HashMap, HashSet};

use rsi_common::types::{Session, SessionKind};
use uuid::Uuid;

use super::SessionState;

const MAX_DISPLAY_LINEAGE_DEPTH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDisplayIdentity {
    pub effective_title: String,
    pub rotation_depth: u32,
    pub effective_epic_ordinal: Option<u32>,
}

fn raw_title(session: &Session) -> String {
    session
        .title
        .as_deref()
        .unwrap_or(&session.query)
        .to_string()
}

/// Resolve title, rotation depth, and effective Epic ordinal from one snapshot.
///
/// Leadership is a virtual presentation overlay: it never rewrites the stored
/// role, ordinal, or raw/manual title. Broken parent or rotation topology
/// degrades to the current row's raw title/query and a blank ordinal.
pub fn resolve_session_display_identity(
    session: &Session,
    all_sessions: &HashMap<Uuid, SessionState>,
) -> SessionDisplayIdentity {
    let mut missing_direct_parent = false;
    let direct_epic = match session.parent_id {
        Some(parent_id) => match all_sessions.get(&parent_id) {
            Some(parent) if parent.session.session_kind == SessionKind::Epic => {
                Some(&parent.session)
            }
            Some(_) => None,
            None => {
                missing_direct_parent = true;
                None
            }
        },
        None => None,
    };

    if let Some(epic) = direct_epic {
        if epic.lead_session_id == Some(session.id) {
            return SessionDisplayIdentity {
                effective_title: "Demiurge".to_string(),
                rotation_depth: session.rotation_depth,
                effective_epic_ordinal: Some(0),
            };
        }
        if let Some(role) = session
            .agent_role
            .as_deref()
            .filter(|role| !role.is_empty())
        {
            return SessionDisplayIdentity {
                effective_title: role.to_string(),
                rotation_depth: session.rotation_depth,
                effective_epic_ordinal: session.epic_spawn_ordinal.filter(|ordinal| *ordinal > 0),
            };
        }
    } else if missing_direct_parent {
        return SessionDisplayIdentity {
            effective_title: raw_title(session),
            rotation_depth: 0,
            effective_epic_ordinal: None,
        };
    }

    let mut current = session;
    let mut depth = 0u32;
    let mut seen = HashSet::from([session.id]);
    for _ in 0..MAX_DISPLAY_LINEAGE_DEPTH {
        let Some(predecessor_id) = current.continued_from else {
            return SessionDisplayIdentity {
                effective_title: raw_title(current),
                rotation_depth: depth,
                effective_epic_ordinal: direct_epic
                    .and_then(|_| session.epic_spawn_ordinal.filter(|ordinal| *ordinal > 0)),
            };
        };
        if !seen.insert(predecessor_id) {
            break;
        }
        let Some(predecessor) = all_sessions.get(&predecessor_id) else {
            break;
        };
        depth = depth.saturating_add(1);
        current = &predecessor.session;
    }

    SessionDisplayIdentity {
        effective_title: raw_title(session),
        rotation_depth: 0,
        effective_epic_ordinal: None,
    }
}

/// A leaf title in the generated `Role: subject` convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleTitle<'a> {
    pub role: &'a str,
    pub subject: &'a str,
}

const MAX_ROLE_WORD_LEN: usize = 20;

fn is_role_word(word: &str) -> bool {
    (2..=MAX_ROLE_WORD_LEN).contains(&word.len())
        && word.chars().all(|ch| ch.is_ascii_alphabetic())
        && word
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
}

/// Split a leaf's effective display title into its role word and subject.
///
/// Presentation-only: the effective title is still decided solely by
/// [`resolve_session_display_identity`]; this only lets renderers draw the
/// role word as a compact code. Recognized shapes are `Role: subject` (any
/// single capitalized word, because the title generator may invent roles) and
/// a bare known role word (`Reviewer`), whose subject is the role word itself
/// so a row never renders empty. Containers always keep their own full name.
pub fn split_role_title(kind: SessionKind, title: &str) -> Option<RoleTitle<'_>> {
    if !rsi_common::is_leaf_kind(kind) {
        return None;
    }
    let trimmed = title.trim();
    if let Some((role, subject)) = trimmed.split_once(':') {
        let role = role.trim();
        let subject = subject.trim();
        if !is_role_word(role) {
            return None;
        }
        return Some(RoleTitle {
            role,
            subject: if subject.is_empty() { role } else { subject },
        });
    }
    crate::ui::glyphs::is_known_role(trimmed).then_some(RoleTitle {
        role: trimmed,
        subject: trimmed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::baseline_session;

    fn insert(map: &mut HashMap<Uuid, SessionState>, session: Session) {
        map.insert(session.id, SessionState::new(session));
    }

    #[test]
    fn role_titles_split_for_leaves_and_leave_containers_whole() {
        assert_eq!(
            split_role_title(SessionKind::Standard, "Manager: Handoff Review"),
            Some(RoleTitle {
                role: "Manager",
                subject: "Handoff Review"
            })
        );
        assert_eq!(
            split_role_title(SessionKind::Task, "Reviewer"),
            Some(RoleTitle {
                role: "Reviewer",
                subject: "Reviewer"
            })
        );
        assert_eq!(
            split_role_title(SessionKind::Standard, "Debugger: 68861f41-c2ac"),
            Some(RoleTitle {
                role: "Debugger",
                subject: "68861f41-c2ac"
            })
        );
        // Containers keep their own identity, even in role-shaped form.
        assert_eq!(
            split_role_title(SessionKind::Group, "Manager: Reliability"),
            None
        );
        // Prose with a colon is not a role title.
        assert_eq!(
            split_role_title(SessionKind::Standard, "fix bug: crash on start"),
            None
        );
        assert_eq!(split_role_title(SessionKind::Standard, "Polish"), None);
    }

    #[test]
    fn session_display_leadership_is_virtual_and_demotion_restores_identity() {
        let epic_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut epic = baseline_session(epic_id, SessionKind::Epic);
        epic.lead_session_id = Some(child_id);
        let mut child = baseline_session(child_id, SessionKind::Task);
        child.parent_id = Some(epic_id);
        child.title = Some("raw manual title".into());
        child.agent_role = Some("Reviewer".into());
        child.epic_spawn_ordinal = Some(7);
        child.rotation_depth = 3;

        let mut map = HashMap::new();
        insert(&mut map, epic.clone());
        insert(&mut map, child.clone());
        assert_eq!(
            resolve_session_display_identity(&child, &map),
            SessionDisplayIdentity {
                effective_title: "Demiurge".into(),
                rotation_depth: 3,
                effective_epic_ordinal: Some(0),
            }
        );
        assert_eq!(child.epic_spawn_ordinal, Some(7));

        epic.lead_session_id = None;
        insert(&mut map, epic);
        assert_eq!(
            resolve_session_display_identity(&child, &map),
            SessionDisplayIdentity {
                effective_title: "Reviewer".into(),
                rotation_depth: 3,
                effective_epic_ordinal: Some(7),
            }
        );
        assert_eq!(child.title.as_deref(), Some("raw manual title"));
    }

    #[test]
    fn session_display_legacy_lineage_and_ordinary_fallback_are_preserved() {
        let epic_id = Uuid::new_v4();
        let root_id = Uuid::new_v4();
        let current_id = Uuid::new_v4();
        let epic = baseline_session(epic_id, SessionKind::Epic);
        let mut root = baseline_session(root_id, SessionKind::Task);
        root.parent_id = Some(epic_id);
        root.title = Some("oldest raw title".into());
        root.epic_spawn_ordinal = Some(2);
        let mut current = baseline_session(current_id, SessionKind::Task);
        current.parent_id = Some(epic_id);
        current.continued_from = Some(root_id);
        current.epic_spawn_ordinal = Some(2);
        current.title = Some("rotation raw title".into());

        let mut map = HashMap::new();
        insert(&mut map, epic);
        insert(&mut map, root);
        insert(&mut map, current.clone());
        assert_eq!(
            resolve_session_display_identity(&current, &map),
            SessionDisplayIdentity {
                effective_title: "oldest raw title".into(),
                rotation_depth: 1,
                effective_epic_ordinal: Some(2),
            }
        );

        let mut ordinary = baseline_session(Uuid::new_v4(), SessionKind::Standard);
        ordinary.title = None;
        ordinary.query = "ordinary query".into();
        assert_eq!(
            resolve_session_display_identity(&ordinary, &map),
            SessionDisplayIdentity {
                effective_title: "ordinary query".into(),
                rotation_depth: 0,
                effective_epic_ordinal: None,
            }
        );
    }

    #[test]
    fn session_display_group_owned_rotation_preserves_root_fallback() {
        let group_id = Uuid::new_v4();
        let root_id = Uuid::new_v4();
        let current_id = Uuid::new_v4();
        let group = baseline_session(group_id, SessionKind::Group);
        let mut root = baseline_session(root_id, SessionKind::Standard);
        root.parent_id = Some(group_id);
        root.title = Some("group-owned root".into());
        let mut current = baseline_session(current_id, SessionKind::Standard);
        current.parent_id = Some(group_id);
        current.continued_from = Some(root_id);
        current.title = Some("rotated title".into());

        let mut map = HashMap::new();
        insert(&mut map, group);
        insert(&mut map, root);
        insert(&mut map, current.clone());

        assert_eq!(
            resolve_session_display_identity(&current, &map),
            SessionDisplayIdentity {
                effective_title: "group-owned root".into(),
                rotation_depth: 1,
                effective_epic_ordinal: None,
            }
        );
    }

    #[test]
    fn session_display_broken_topology_fails_closed_to_current_raw_identity() {
        let mut session = baseline_session(Uuid::new_v4(), SessionKind::Task);
        session.title = Some("current raw".into());
        session.parent_id = Some(Uuid::new_v4());
        session.agent_role = Some("must not escape missing parent".into());
        session.epic_spawn_ordinal = Some(9);
        let map = HashMap::new();
        assert_eq!(
            resolve_session_display_identity(&session, &map),
            SessionDisplayIdentity {
                effective_title: "current raw".into(),
                rotation_depth: 0,
                effective_epic_ordinal: None,
            }
        );

        session.parent_id = None;
        session.continued_from = Some(session.id);
        assert_eq!(
            resolve_session_display_identity(&session, &map),
            SessionDisplayIdentity {
                effective_title: "current raw".into(),
                rotation_depth: 0,
                effective_epic_ordinal: None,
            }
        );
    }
}
