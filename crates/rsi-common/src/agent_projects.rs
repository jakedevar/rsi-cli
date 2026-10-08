//! `AgentCreateProject` and `AgentUpdateProject` (#1626 slice 1): wire types
//! for an agent that registers or edits an RSI project on the operator's
//! behalf.
//!
//! Who may call: an appointed project manager (its own project, Execute mode,
//! not paused) or a portfolio seat (its grant's coverage, Execute mode, not
//! paused). The daemon resolves the caller from its token; nothing here names
//! a caller. There is deliberately no agent verb that deletes or archives a
//! project: the `projects` table has no archive state, and a hard delete is
//! the operator's (AGENTS.md hard rule 3).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROJECT_NAME_MAX_CHARS: usize = 128;
pub const PROJECT_PATH_MAX_BYTES: usize = 4096;
pub const PROJECT_DESCRIPTION_MAX_CHARS: usize = 2048;

/// The request is malformed (unknown field, bad type, empty update).
pub const PROJECT_INVALID_REQUEST: &str = "project_invalid_request";
/// The name is empty, too long or carries a control character.
pub const PROJECT_NAME_INVALID: &str = "project_name_invalid";
/// The path is empty, relative, too long, missing, not a directory or outside
/// the allowed area: a configured workspace root, or, with none configured, a
/// strict descendant of the daemon user's home directory that is not under
/// the RSI data directory (`~/.rsi`).
pub const PROJECT_PATH_INVALID: &str = "project_path_invalid";
/// The color is not `#rrggbb`.
pub const PROJECT_COLOR_INVALID: &str = "project_color_invalid";
/// The description is too long.
pub const PROJECT_DESCRIPTION_INVALID: &str = "project_description_invalid";
/// The caller holds no seat that may administer projects, or its policy is
/// not in Execute mode, or it is paused.
pub const PROJECT_NOT_AUTHORIZED: &str = "project_not_authorized";
/// Another project already uses this name (names are unique), or the same
/// name and directory belong to a project outside the caller's coverage (a
/// replay is answered only inside coverage).
pub const PROJECT_NAME_TAKEN: &str = "project_name_taken";
/// Another project already registers this directory.
pub const PROJECT_PATH_TAKEN: &str = "project_path_taken";
/// The path equals, contains or sits inside the daemon's harness root, or the
/// edit would move the harness project: only the operator changes that binding.
pub const PROJECT_HARNESS_PROTECTED: &str = "project_harness_protected";
/// The project is not in the caller's coverage (also returned for a project
/// that does not exist, so the answer does not reveal other projects).
pub const PROJECT_NOT_IN_SCOPE: &str = "project_not_in_scope";
/// A path change was refused because the project has a live session.
pub const PROJECT_HAS_LIVE_SESSIONS: &str = "project_has_live_sessions";
/// This daemon handle cannot refresh project caches (a rotation-built handle);
/// call again from a fresh session or use the RPC.
pub const PROJECT_ADMIN_UNAVAILABLE: &str = "project_admin_unavailable";

/// Bus event type published when an agent registers a project (slice 2).
pub const PROJECT_CREATED_EVENT: &str = "project_created";
/// `source` carried by `project_created`: only agent creates publish it.
pub const PROJECT_CREATED_SOURCE_AGENT: &str = "agent";

/// Payload of the `project_created` bus event (`BusEvent.data`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCreatedEventV1 {
    pub project_id: Uuid,
    pub name: String,
    pub source: String,
    /// The creating agent session.
    pub created_by_session_id: Uuid,
}

/// `AgentCreateProject {name, path, description?, color?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCreateProjectRequestV1 {
    pub name: String,
    /// Absolute directory of the project's repository or working tree.
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `#rrggbb`; the daemon default when omitted.
    #[serde(default)]
    pub color: Option<String>,
}

impl AgentCreateProjectRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_name(&self.name)?;
        validate_path(&self.path)?;
        validate_description(self.description.as_deref())?;
        validate_color(self.color.as_deref())
    }
}

/// `AgentUpdateProject {project_id, name?, path?, description?, color?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUpdateProjectRequestV1 {
    /// A project in your coverage.
    pub project_id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

impl AgentUpdateProjectRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.is_none()
            && self.path.is_none()
            && self.description.is_none()
            && self.color.is_none()
        {
            return Err(PROJECT_INVALID_REQUEST);
        }
        if let Some(name) = &self.name {
            validate_name(name)?;
        }
        if let Some(path) = &self.path {
            validate_path(path)?;
        }
        validate_description(self.description.as_deref())?;
        validate_color(self.color.as_deref())
    }
}

/// The project as an agent sees it after a create or update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProjectResultV1 {
    pub project_id: Uuid,
    pub name: String,
    pub path: Option<String>,
    pub description: Option<String>,
    pub color: String,
    /// Create only: `true` when the same name and directory were already
    /// registered (a replay), so nothing changed.
    pub deduplicated: bool,
}

fn validate_name(name: &str) -> Result<(), &'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > PROJECT_NAME_MAX_CHARS
        || trimmed.chars().any(char::is_control)
    {
        return Err(PROJECT_NAME_INVALID);
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty()
        || path.len() > PROJECT_PATH_MAX_BYTES
        || path.contains('\0')
        || !std::path::Path::new(path).is_absolute()
    {
        return Err(PROJECT_PATH_INVALID);
    }
    Ok(())
}

fn validate_description(description: Option<&str>) -> Result<(), &'static str> {
    match description {
        Some(text) if text.chars().count() > PROJECT_DESCRIPTION_MAX_CHARS => {
            Err(PROJECT_DESCRIPTION_INVALID)
        }
        _ => Ok(()),
    }
}

fn validate_color(color: Option<&str>) -> Result<(), &'static str> {
    match color {
        Some(value)
            if !(value.len() == 7
                && value.starts_with('#')
                && value[1..].chars().all(|c| c.is_ascii_hexdigit())) =>
        {
            Err(PROJECT_COLOR_INVALID)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_requires_absolute_path_and_clean_name() {
        let ok = AgentCreateProjectRequestV1 {
            name: "Demo".into(),
            path: "/tmp/demo".into(),
            description: None,
            color: Some("#aabbcc".into()),
        };
        assert_eq!(ok.validate(), Ok(()));
        let relative = AgentCreateProjectRequestV1 {
            path: "demo".into(),
            ..ok.clone()
        };
        assert_eq!(relative.validate(), Err(PROJECT_PATH_INVALID));
        let blank = AgentCreateProjectRequestV1 {
            name: "  ".into(),
            ..ok.clone()
        };
        assert_eq!(blank.validate(), Err(PROJECT_NAME_INVALID));
        let colored = AgentCreateProjectRequestV1 {
            color: Some("blue".into()),
            ..ok
        };
        assert_eq!(colored.validate(), Err(PROJECT_COLOR_INVALID));
    }

    #[test]
    fn update_needs_at_least_one_change() {
        let empty = AgentUpdateProjectRequestV1 {
            project_id: Uuid::nil(),
            name: None,
            path: None,
            description: None,
            color: None,
        };
        assert_eq!(empty.validate(), Err(PROJECT_INVALID_REQUEST));
        let rename = AgentUpdateProjectRequestV1 {
            name: Some("Renamed".into()),
            ..empty
        };
        assert_eq!(rename.validate(), Ok(()));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = serde_json::from_value::<AgentCreateProjectRequestV1>(serde_json::json!({
            "name": "Demo", "path": "/tmp/demo", "project_id": Uuid::nil()
        }));
        assert!(error.is_err());
    }
}
