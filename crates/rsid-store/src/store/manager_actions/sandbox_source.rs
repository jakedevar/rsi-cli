//! #1195: the sandbox source of a manager `create_session` (and so of
//! `AgentManagerLaunchIssueWorker`). Workers used to fork from the project
//! checkout's `HEAD`, which carries whatever the operator has checked out,
//! including unpublished commits. A create now records an explicit source on
//! its frozen fork source:
//!
//! - `rolling` (the default): resolved at launch to the freshly fetched
//!   `origin/rolling` tip (`RollingBasePolicy::PublishedTip`), recorded as
//!   the allocation's source commit (`base_commit` in progress);
//! - `commit`: that exact commit of the project repository;
//! - `path`: the committed `HEAD` of a registered worktree of the project
//!   repository, pinned here, at admission (and at prepare).
//!
//! Every refusal happens here, before the action is journalled.

use super::{ManagerActionSourceV2, ManagerFrozenSandboxSourceV1};
use crate::error::Result;
use crate::store::Store;
use crate::store::harness_manager_v2::refused;
use rsi_common::harness_manager_v2::{
    MANAGER_SANDBOX_SOURCE_COMMIT_UNKNOWN, MANAGER_SANDBOX_SOURCE_NOT_WORKTREE,
    ManagerSandboxSourceKindV1, ManagerSandboxSourceReceiptV1, ManagerSandboxSourceV1,
};
use rsi_common::types::Session;
use std::path::Path;

impl Store {
    /// Freeze the fork source of a `create_session` under `source` (its parent
    /// container) at the requested sandbox source. Unlike
    /// [`Self::manager_action_freeze_source`] the container checkout need not
    /// be clean: none of its uncommitted state is used.
    pub(super) fn manager_action_selected_source(
        &self,
        source: &Session,
        requested: Option<&ManagerSandboxSourceV1>,
    ) -> Result<ManagerActionSourceV2> {
        if let Some(requested) = requested {
            requested.validate().map_err(refused)?;
        }
        let root = self.manager_action_source_root(source)?;
        let (commit, selection) = match requested.unwrap_or(&ManagerSandboxSourceV1::Rolling {}) {
            ManagerSandboxSourceV1::Rolling {} => (
                crate::sandbox::git_worktree::observe_head_bounded(&root)?,
                ManagerFrozenSandboxSourceV1::Rolling,
            ),
            ManagerSandboxSourceV1::Commit(commit) => {
                crate::sandbox::git_worktree::require_commit_object_bounded(&root, commit)
                    .map_err(|_| refused(MANAGER_SANDBOX_SOURCE_COMMIT_UNKNOWN))?;
                (commit.clone(), ManagerFrozenSandboxSourceV1::Commit)
            }
            ManagerSandboxSourceV1::Path(path) => {
                let (commit, source_dirty) =
                    crate::sandbox::git_worktree::resolve_registered_worktree_head(
                        &root,
                        Path::new(path),
                    )?
                    .ok_or_else(|| refused(MANAGER_SANDBOX_SOURCE_NOT_WORKTREE))?;
                (
                    commit,
                    ManagerFrozenSandboxSourceV1::Path {
                        path: path.into(),
                        source_dirty,
                    },
                )
            }
        };
        Ok(ManagerActionSourceV2 {
            session_id: source.id,
            working_dir: source.working_dir.clone(),
            sandbox_root: source.sandbox_root.clone(),
            commit,
            custody_generation: self.manager_action_custody_generation(source.id)?,
            historical_commit: false,
            branch: None,
            sandbox_source: Some(selection),
        })
    }
}

/// The receipt projection of a frozen create source.
pub(super) fn sandbox_source_receipt(
    source: &ManagerActionSourceV2,
) -> Option<ManagerSandboxSourceReceiptV1> {
    let (kind, commit, source_dirty) = match source.sandbox_source.as_ref()? {
        ManagerFrozenSandboxSourceV1::Rolling => (ManagerSandboxSourceKindV1::Rolling, None, false),
        ManagerFrozenSandboxSourceV1::Commit => (
            ManagerSandboxSourceKindV1::Commit,
            Some(source.commit.clone()),
            false,
        ),
        ManagerFrozenSandboxSourceV1::Path { source_dirty, .. } => (
            ManagerSandboxSourceKindV1::Path,
            Some(source.commit.clone()),
            *source_dirty,
        ),
    };
    Some(ManagerSandboxSourceReceiptV1 {
        kind,
        commit,
        source_dirty,
    })
}
