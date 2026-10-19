//! Short ids on read verbs (#2312): resolve the id prefix a notice or ledger
//! prints (8 hex characters) to the one full id inside the caller's scope
//! before `AgentGetJob`, `AgentGetStatus`, `AgentGetProgress` or
//! `AgentReadSessionEvents` decode their params.
//!
//! The scope is the verb's own: a job `AgentGetJob` reads (the caller's own, or
//! for a manager seat a predecessor's or a scoped worker's, #139), a
//! session is one `authorize_agent_target` admits (`AgentGetStatus`' scope).
//! Only candidates inside that scope count, so an id outside it is refused as
//! "not found" exactly like an id that does not exist, and an ambiguity
//! refusal names only ids the caller may already read.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::agent_id_ref::{
    ID_PREFIX_AMBIGUOUS, ID_PREFIX_CANDIDATES_MAX, ID_PREFIX_MIN, ID_PREFIX_NOT_FOUND,
    ID_PREFIX_TOO_SHORT, IdRef, IdRefKind, classify_id_ref, id_prefixes_in, replace_id_prefixes,
};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

/// Rows read for one prefix before scoping (a prefix of 8 hex characters
/// collides with another id only by accident).
const PREFIX_SCAN_LIMIT: usize = 64;

impl AgentControlHandle {
    /// `params` with every id prefix of `verb`'s id fields replaced by the full
    /// id it names inside `caller`'s scope. Params without a prefix come back
    /// unchanged; a verb with no prefix-capable id field is returned as is.
    ///
    /// # Errors
    /// `id_prefix_too_short`, `id_prefix_not_found` (unknown and out of scope,
    /// identical; jobs say `job_not_found`) or `id_prefix_ambiguous: <prefix>
    /// matches N in your scope: <ids>`.
    pub async fn expand_id_prefixes(
        &self,
        caller: Uuid,
        verb: AgentControlVerbV1,
        mut params: Value,
    ) -> Result<Value> {
        let prefixes = id_prefixes_in(verb, &params);
        if prefixes.is_empty() {
            return Ok(params);
        }
        let mut resolved = HashMap::new();
        for (kind, text) in prefixes {
            let id = self.resolve_id_prefix(caller, kind, &text).await?;
            resolved.insert(text, id);
        }
        replace_id_prefixes(verb, &mut params, &resolved);
        Ok(params)
    }

    async fn resolve_id_prefix(&self, caller: Uuid, kind: IdRefKind, text: &str) -> Result<Uuid> {
        let hex = match classify_id_ref(text) {
            IdRef::Full(id) => return Ok(id),
            IdRef::Prefix(hex) => hex,
            IdRef::TooShort => {
                return Err(DaemonError::InvalidParam(format!(
                    "{ID_PREFIX_TOO_SHORT}: use a full id or at least {ID_PREFIX_MIN} hex characters"
                )));
            }
            IdRef::Invalid => {
                return Err(DaemonError::InvalidParam(format!(
                    "{ID_PREFIX_NOT_FOUND}: not an id or id prefix"
                )));
            }
        };
        let mut matches = self.scoped_ids_with_prefix(caller, kind, &hex).await?;
        matches.sort();
        matches.dedup();
        match matches.as_slice() {
            [] => Err(DaemonError::InvalidParam(match kind {
                IdRefKind::Job => rsi_common::agent_jobs::JOB_NOT_FOUND.into(),
                IdRefKind::Session => format!(
                    "{ID_PREFIX_NOT_FOUND}: no session in your scope has an id starting with {hex}"
                ),
            })),
            [only] => Ok(*only),
            many => {
                let shown: Vec<String> = many
                    .iter()
                    .take(ID_PREFIX_CANDIDATES_MAX)
                    .map(Uuid::to_string)
                    .collect();
                let more = many.len().saturating_sub(shown.len());
                Err(DaemonError::InvalidParam(format!(
                    "{ID_PREFIX_AMBIGUOUS}: {hex} matches {} in your scope: {}{}",
                    many.len(),
                    shown.join(", "),
                    if more > 0 {
                        format!(" (+{more} more)")
                    } else {
                        String::new()
                    }
                )))
            }
        }
    }

    /// Every id of `kind` that starts with `hex` and that `caller` may read.
    async fn scoped_ids_with_prefix(
        &self,
        caller: Uuid,
        kind: IdRefKind,
        hex: &str,
    ) -> Result<Vec<Uuid>> {
        match kind {
            IdRefKind::Job => {
                // #139: the scope is the one `AgentGetJob` reads by full id
                // (`get_agent_job_for_reader`): the caller's own jobs, and for
                // a manager seat its rotation predecessors' jobs and those of
                // sessions in its live scope. An owner-only filter refused a
                // manager's prefix of a worker's job that its full id reads.
                let store = self.store.lock().await;
                let mut reached = Vec::new();
                for (job, _owner) in store.agent_job_owners_by_id_prefix(hex, PREFIX_SCAN_LIMIT)? {
                    if store.get_agent_job_for_reader(caller, job)?.is_some() {
                        reached.push(job);
                    }
                }
                Ok(reached)
            }
            IdRefKind::Session => {
                let candidates = self
                    .store
                    .lock()
                    .await
                    .session_ids_by_id_prefix(hex, PREFIX_SCAN_LIMIT)?;
                let mut reached = Vec::new();
                for id in candidates {
                    if self.authorize_agent_target(caller, id).await.is_ok() {
                        reached.push(id);
                    }
                }
                Ok(reached)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_verbs::tests::{control_handle_with_store, test_session};
    use super::*;
    use crate::store::Store;
    use crate::store::agent_jobs::NewAgentJob;
    use chrono::Utc;
    use rsi_common::agent_jobs::{BuildCommand, BuildJobParams, JobParams, JobWake};
    use rsi_common::types::{SessionKind, SessionStatus};
    use serde_json::json;
    use std::sync::Arc;

    type SharedStore = Arc<tokio::sync::Mutex<Store>>;

    fn id(text: &str) -> Uuid {
        Uuid::parse_str(text).expect("uuid")
    }

    async fn session(store: &SharedStore, id: Uuid, parent: Option<Uuid>) {
        let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.parent_id = parent;
        store.lock().await.insert_session(&row).expect("insert");
    }

    async fn job(store: &SharedStore, job_id: Uuid, owner: Uuid) {
        let new = NewAgentJob {
            id: job_id,
            owner_session_id: owner,
            project_id: None,
            name: None,
            params: JobParams::Build(BuildJobParams {
                command: BuildCommand::Check,
                package: None,
                workspace: true,
                all_targets: false,
                release: false,
                no_default_features: false,
                features: Vec::new(),
                shards: Vec::new(),
                cpu_quota_percent: None,
                sha: None,
            }),
            cwd: "/tmp".into(),
            unit_name: format!("rsi-job-{job_id}"),
            log_path: format!("/tmp/{job_id}.log"),
            status_path: format!("/tmp/{job_id}.status"),
            idempotency_key: None,
            wake: JobWake::Owner,
        };
        store
            .lock()
            .await
            .insert_agent_job(&new, Utc::now())
            .expect("job");
    }

    async fn expand(
        control: &AgentControlHandle,
        caller: Uuid,
        verb: AgentControlVerbV1,
        params: Value,
    ) -> Result<Value> {
        control.expand_id_prefixes(caller, verb, params).await
    }

    const JOB_A: &str = "0e367273-1111-4222-8333-444455556666";
    const JOB_B: &str = "0e367273-9999-4222-8333-444455556666";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_unique_job_prefix_resolves_to_the_full_id() {
        let (control, store) = control_handle_with_store();
        let owner = Uuid::new_v4();
        session(&store, owner, None).await;
        job(&store, id(JOB_A), owner).await;

        let out = expand(
            &control,
            owner,
            AgentControlVerbV1::GetJob,
            json!({"job_id": "0e367273"}),
        )
        .await
        .expect("prefix resolves");
        assert_eq!(out, json!({"job_id": JOB_A}));
        // A full id, with or without a prefix elsewhere, passes through untouched.
        let full = json!({"job_id": JOB_A});
        let same = expand(&control, owner, AgentControlVerbV1::GetJob, full.clone())
            .await
            .expect("full id");
        assert_eq!(same, full);
        // The read itself accepts the resolved id.
        let read = control
            .agent_get_job(owner, serde_json::from_value(out).expect("typed request"))
            .await
            .expect("job");
        assert_eq!(read.id, id(JOB_A));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn an_ambiguous_job_prefix_is_refused_with_its_candidates() {
        let (control, store) = control_handle_with_store();
        let owner = Uuid::new_v4();
        session(&store, owner, None).await;
        job(&store, id(JOB_A), owner).await;
        job(&store, id(JOB_B), owner).await;

        let error = expand(
            &control,
            owner,
            AgentControlVerbV1::GetJob,
            json!({"job_id": "0e367273"}),
        )
        .await
        .expect_err("two jobs share the prefix")
        .to_string();
        assert!(error.contains(ID_PREFIX_AMBIGUOUS), "{error}");
        assert!(error.contains(JOB_A) && error.contains(JOB_B), "{error}");
        // A longer prefix disambiguates.
        let out = expand(
            &control,
            owner,
            AgentControlVerbV1::GetJob,
            json!({"job_id": "0e367273-9999"}),
        )
        .await
        .expect("longer prefix");
        assert_eq!(out, json!({"job_id": JOB_B}));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_job_outside_the_callers_scope_is_not_disclosed() {
        let (control, store) = control_handle_with_store();
        let owner = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        session(&store, owner, None).await;
        session(&store, stranger, None).await;
        job(&store, id(JOB_A), owner).await;
        job(&store, id(JOB_B), stranger).await;

        // The stranger's own job resolves; the owner's job (same prefix) is
        // neither counted as a candidate nor named.
        let own = expand(
            &control,
            stranger,
            AgentControlVerbV1::GetJob,
            json!({"job_id": "0e367273"}),
        )
        .await
        .expect("only the stranger's own job is in scope");
        assert_eq!(own, json!({"job_id": JOB_B}));

        // A prefix that only a foreign job matches is refused like one that
        // matches nothing.
        let foreign = expand(
            &control,
            stranger,
            AgentControlVerbV1::GetJob,
            json!({"job_id": "0e367273-1111"}),
        )
        .await
        .expect_err("foreign job")
        .to_string();
        let unknown = expand(
            &control,
            stranger,
            AgentControlVerbV1::GetJob,
            json!({"job_id": "ffffffff-1111"}),
        )
        .await
        .expect_err("unknown job")
        .to_string();
        assert_eq!(foreign, unknown);
        assert!(!foreign.contains(JOB_A), "{foreign}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn session_prefixes_resolve_inside_status_scope_only() {
        let (control, store) = control_handle_with_store();
        let parent = Uuid::new_v4();
        let child = id("aabbccdd-0000-4000-8000-000000000001");
        let stranger = Uuid::new_v4();
        session(&store, parent, None).await;
        session(&store, child, Some(parent)).await;
        session(&store, stranger, None).await;

        let out = expand(
            &control,
            parent,
            AgentControlVerbV1::GetStatus,
            json!({"session_id": "aabbccdd"}),
        )
        .await
        .expect("a parent reaches its child");
        assert_eq!(out, json!({"session_id": child.to_string()}));
        let events = expand(
            &control,
            parent,
            AgentControlVerbV1::ReadSessionEvents,
            json!({"session_id": "aabbccdd", "limit": 5}),
        )
        .await
        .expect("events");
        assert_eq!(events["session_id"], child.to_string());
        assert_eq!(events["limit"], 5);

        let hidden = expand(
            &control,
            stranger,
            AgentControlVerbV1::GetStatus,
            json!({"session_id": "aabbccdd"}),
        )
        .await
        .expect_err("out of scope")
        .to_string();
        let unknown = expand(
            &control,
            stranger,
            AgentControlVerbV1::GetStatus,
            json!({"session_id": "ffffffff"}),
        )
        .await
        .expect_err("unknown")
        .to_string();
        assert!(hidden.contains(ID_PREFIX_NOT_FOUND), "{hidden}");
        assert_eq!(
            hidden.replace("aabbccdd", "X"),
            unknown.replace("ffffffff", "X")
        );
        assert!(!hidden.contains(&child.to_string()), "{hidden}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn an_ambiguous_session_prefix_names_only_candidates_in_scope() {
        let (control, store) = control_handle_with_store();
        let parent = Uuid::new_v4();
        let first = id("aabbccdd-0000-4000-8000-000000000001");
        let second = id("aabbccdd-0000-4000-8000-000000000002");
        let foreign = id("aabbccdd-0000-4000-8000-000000000003");
        session(&store, parent, None).await;
        session(&store, first, Some(parent)).await;
        session(&store, second, Some(parent)).await;
        session(&store, foreign, None).await;

        let error = expand(
            &control,
            parent,
            AgentControlVerbV1::GetProgress,
            json!({"session_ids": ["aabbccdd"]}),
        )
        .await
        .expect_err("two children share the prefix")
        .to_string();
        assert!(error.contains(ID_PREFIX_AMBIGUOUS), "{error}");
        assert!(
            error.contains(&first.to_string()) && error.contains(&second.to_string()),
            "{error}"
        );
        assert!(!error.contains(&foreign.to_string()), "{error}");

        // A longer prefix resolves, and a progress cohort mixes full ids.
        let full = Uuid::new_v4().to_string();
        let out = expand(
            &control,
            parent,
            AgentControlVerbV1::GetProgress,
            json!({"session_ids": ["aabbccdd-0000-4000-8000-000000000002", full]}),
        )
        .await
        .expect("full ids pass through");
        assert_eq!(out["session_ids"][0], second.to_string());
        assert_eq!(out["session_ids"][1], full);
        let short = expand(
            &control,
            parent,
            AgentControlVerbV1::GetProgress,
            json!({"session_ids": ["aabbccdd-0000-4000-8000-0000000000"]}),
        )
        .await
        .expect_err("still two");
        assert!(short.to_string().contains(ID_PREFIX_AMBIGUOUS));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_prefix_under_eight_hex_characters_is_refused() {
        let (control, store) = control_handle_with_store();
        let owner = Uuid::new_v4();
        session(&store, owner, None).await;
        let error = expand(
            &control,
            owner,
            AgentControlVerbV1::GetStatus,
            json!({"session_id": "0e3672"}),
        )
        .await
        .expect_err("too short")
        .to_string();
        assert!(error.contains(ID_PREFIX_TOO_SHORT), "{error}");
        // A verb that mutates is never expanded: its params are returned as is.
        let halt = json!({"session_id": "0e367273"});
        let same = expand(&control, owner, AgentControlVerbV1::Halt, halt.clone())
            .await
            .expect("not a prefix verb");
        assert_eq!(same, halt);
    }
}
