//! Launch-time resolution of a Harness session's exec target (#3176).
//!
//! Every launch path (fresh, continue, retry, restart resume, rotation)
//! resolves the target before any provider effect and validates a container
//! target against the live docker host. A container-bound session never falls
//! back to the host: an unreadable binding or a container that cannot be
//! rebuilt refuses with `exec_target_unavailable`; a target that fails a check
//! refuses with `exec_target_invalid:<reason>`.

use crate::error::DaemonError;
use crate::store::Store;
use rsi_common::exec_target::{EXEC_TARGET_UNAVAILABLE, ExecTarget};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Validation reasons that mean "the container cannot be reached or is gone"
/// rather than "the target is wrong". On a continuation, resume or rotation
/// they read as `exec_target_unavailable` (the same target cannot be rebuilt).
const UNAVAILABLE_REASONS: &[&str] = &[
    "inspect_failed",
    "inspect_timed_out",
    "inspect_malformed",
    "container_not_running",
];

fn unavailable(reason: &str) -> DaemonError {
    DaemonError::PolicyDenied(format!("{EXEC_TARGET_UNAVAILABLE}:{reason}"))
}

/// The target `session_id` launches against, validated.
///
/// A row already stored for `session_id` wins (a retry relaunch cannot
/// redirect it). Otherwise `explicit` (a daemon-built container target of a
/// fresh launch) applies. Otherwise the binding resolves through the
/// `continued_from` chain of `session_id`, then of `parent`; a continued or
/// rotated session therefore needs no copy of its ancestor's row. A session
/// with none runs on the host.
///
/// # Errors
/// `exec_target_invalid:<reason>` when the container target fails a check,
/// `exec_target_unavailable:<reason>` when the binding cannot be read or the
/// bound container cannot be reached again.
pub(super) async fn resolve_launch_exec_target(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    runtime_config: &crate::config::RuntimeConfig,
    session_id: Uuid,
    parent: Option<Uuid>,
    explicit: &ExecTarget,
) -> crate::error::Result<ExecTarget> {
    let (target, rebuilt) = {
        let store = store.lock().await;
        let read = (|| -> crate::error::Result<(ExecTarget, bool)> {
            if let Some(stored) = store.get_session_exec_target(session_id)? {
                return Ok((stored, true));
            }
            if explicit.is_container() {
                return Ok((explicit.clone(), false));
            }
            let mut found = store.resolve_session_exec_target(session_id)?;
            if found.is_none()
                && let Some(parent) = parent
            {
                found = store.resolve_session_exec_target(parent)?;
            }
            Ok(found.map_or((ExecTarget::Host, false), |target| (target, true)))
        })();
        match read {
            Ok(pair) => pair,
            Err(error) => {
                tracing::error!(%session_id, %error, "exec target unreadable; refusing the launch");
                return Err(unavailable("binding_unreadable"));
            }
        }
    };
    let ExecTarget::Container(container) = &target else {
        return Ok(target);
    };
    let allowed = runtime_config.bench_docker_hosts();
    match super::harness::tools::exec_target::validate_container_target(
        container,
        &allowed,
        &CancellationToken::new(),
    )
    .await
    {
        Ok(()) => Ok(target),
        Err(code) => {
            let reason = code
                .strip_prefix(&format!(
                    "{}:",
                    rsi_common::exec_target::EXEC_TARGET_INVALID
                ))
                .unwrap_or(&code);
            if rebuilt && UNAVAILABLE_REASONS.contains(&reason) {
                Err(unavailable(reason))
            } else {
                Err(DaemonError::PolicyDenied(code))
            }
        }
    }
}

/// Record a daemon-built container target for `session_id`. The session row must
/// already exist (foreign key); an existing row is kept.
pub(super) async fn persist_launch_exec_target(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    session_id: Uuid,
    target: &ExecTarget,
) -> crate::error::Result<()> {
    store
        .lock()
        .await
        .insert_session_exec_target(session_id, target, chrono::Utc::now())
        .map(drop)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::session::harness::tools::exec_target::test_support::{
        Exec, PROJECT, Shim, container_json, target,
    };
    use rsi_common::exec_target::ContainerExecTarget;

    fn store() -> Arc<rsid_core::store_lock::Mutex<Store>> {
        Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ))
    }

    fn runtime(hosts: &str) -> Arc<crate::config::RuntimeConfig> {
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .update_field("bench.docker_hosts", &serde_json::json!(hosts))
            .unwrap();
        runtime
    }

    async fn session(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        continued_from: Option<Uuid>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let mut row = crate::session::agent_verbs::tests::test_session(
            id,
            std::path::PathBuf::from("/tmp/exec-target-launch"),
        );
        row.continued_from = continued_from;
        store.lock().await.insert_session(&row).unwrap();
        id
    }

    fn container(target: ContainerExecTarget) -> ExecTarget {
        ExecTarget::Container(target)
    }

    async fn refusal(result: crate::error::Result<ExecTarget>) -> String {
        match result {
            Err(DaemonError::PolicyDenied(code)) => code,
            other => panic!("expected a typed refusal, got {other:?}"),
        }
    }

    /// A fresh launch validates and persists the binding; a continuation, a
    /// rotation child and a restart resume (each with no explicit target of
    /// their own) rebuild exactly the same container.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn the_binding_is_rebuilt_by_continuation_rotation_and_resume() {
        let _shim = Shim::new(Exec::Inert);
        let store = store();
        let runtime = runtime("ssh://arch-laptop");
        let root = session(&store, None).await;
        let bound = container(target());
        let fresh = resolve_launch_exec_target(&store, &runtime, root, None, &bound)
            .await
            .expect("a valid target binds");
        assert_eq!(fresh, bound);
        persist_launch_exec_target(&store, root, &fresh)
            .await
            .unwrap();

        // Continue / restart resume: the same id, no explicit target.
        let resumed =
            resolve_launch_exec_target(&store, &runtime, root, None, &ExecTarget::Host).await;
        assert_eq!(resumed.unwrap(), bound);
        // Rotation child: a new session whose row names the parent.
        let child = session(&store, Some(root)).await;
        let grandchild = session(&store, Some(child)).await;
        for id in [child, grandchild] {
            let rotated =
                resolve_launch_exec_target(&store, &runtime, id, None, &ExecTarget::Host).await;
            assert_eq!(rotated.unwrap(), bound, "{id}");
        }
        // A child with no row yet resolves through its parent.
        let unborn = resolve_launch_exec_target(
            &store,
            &runtime,
            Uuid::new_v4(),
            Some(root),
            &ExecTarget::Host,
        )
        .await;
        assert_eq!(unborn.unwrap(), bound);
        // A retry cannot redirect a stored binding.
        let mut elsewhere = target();
        elsewhere.container = "someone-elses-container".into();
        let retried =
            resolve_launch_exec_target(&store, &runtime, root, None, &container(elsewhere)).await;
        assert_eq!(retried.unwrap(), bound);
    }

    /// A session with no binding is a host session and never contacts docker.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_unbound_session_runs_on_the_host_without_touching_docker() {
        let shim = Shim::new(Exec::Inert);
        let store = store();
        let id = session(&store, None).await;
        let resolved =
            resolve_launch_exec_target(&store, &runtime(""), id, None, &ExecTarget::Host).await;
        assert_eq!(resolved.unwrap(), ExecTarget::Host);
        assert!(shim.invocations().is_empty());
    }

    /// The operator allowlist is checked at every launch and continuation: an
    /// empty list refuses a fresh target, and removing the host refuses a bound
    /// session's next continuation as invalid (not as unavailable).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn the_allowlist_is_enforced_before_binding_and_on_every_continuation() {
        let shim = Shim::new(Exec::Inert);
        let store = store();
        let root = session(&store, None).await;
        let bound = container(target());
        let code =
            refusal(resolve_launch_exec_target(&store, &runtime(""), root, None, &bound).await)
                .await;
        assert_eq!(code, "exec_target_invalid:docker_host_not_allowed");
        assert!(shim.invocations().is_empty(), "docker was contacted");

        persist_launch_exec_target(&store, root, &bound)
            .await
            .unwrap();
        for hosts in ["", "ssh://elsewhere"] {
            let code = refusal(
                resolve_launch_exec_target(&store, &runtime(hosts), root, None, &ExecTarget::Host)
                    .await,
            )
            .await;
            assert_eq!(
                code, "exec_target_invalid:docker_host_not_allowed",
                "{hosts}"
            );
        }
    }

    /// A target that fails a live check is invalid on a fresh launch. The same
    /// fact on a continuation (the container is gone, stopped or unreachable)
    /// is `exec_target_unavailable`; a wrong label or digest stays invalid.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_bound_session_refuses_when_its_container_cannot_be_rebuilt() {
        let shim = Shim::new(Exec::Inert);
        let store = store();
        let runtime = runtime("ssh://arch-laptop");
        let bound = container(target());
        let fresh = session(&store, None).await;

        shim.set_container_json(&container_json(false, "main", PROJECT));
        let code =
            refusal(resolve_launch_exec_target(&store, &runtime, fresh, None, &bound).await).await;
        assert_eq!(code, "exec_target_invalid:container_not_running");

        let root = session(&store, None).await;
        persist_launch_exec_target(&store, root, &bound)
            .await
            .unwrap();
        let child = session(&store, Some(root)).await;
        let continued =
            |id| resolve_launch_exec_target(&store, &runtime, id, None, &ExecTarget::Host);

        let code = refusal(continued(child).await).await;
        assert_eq!(code, "exec_target_unavailable:container_not_running");
        shim.set_container_json("[]");
        let code = refusal(continued(root).await).await;
        assert_eq!(code, "exec_target_unavailable:inspect_malformed");
        shim.fail("container");
        let code = refusal(continued(child).await).await;
        assert_eq!(code, "exec_target_unavailable:inspect_failed");
        std::fs::remove_file(shim.path("container.fail")).unwrap();
        shim.set_container_json(&container_json(true, "main", "another-project"));
        let code = refusal(continued(child).await).await;
        assert_eq!(code, "exec_target_invalid:project_label_mismatch");
        shim.set_container_json(&container_json(true, "main", PROJECT));
        shim.set_image_json(r#"[{"RepoDigests":["r/x@sha256:0000000000000000000000000000000000000000000000000000000000000000"]}]"#);
        let code = refusal(continued(child).await).await;
        assert_eq!(code, "exec_target_invalid:image_digest_not_pinned");
        // Once the container is back it rebuilds.
        shim.set_image_json(
            &crate::session::harness::tools::exec_target::test_support::image_json(
                crate::session::harness::tools::exec_target::test_support::PINNED,
            ),
        );
        assert_eq!(continued(child).await.unwrap(), bound);
    }

    /// An unreadable binding refuses; it is never read as "no binding".
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_unreadable_binding_refuses_instead_of_running_on_the_host() {
        let _shim = Shim::new(Exec::Inert);
        let store = store();
        let root = session(&store, None).await;
        persist_launch_exec_target(&store, root, &container(target()))
            .await
            .unwrap();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute_batch(
                    "DROP TRIGGER session_exec_targets_no_update;
                     UPDATE session_exec_targets SET target_json='{\"kind\":\"nope\"}';",
                )
                .unwrap();
        }
        let child = session(&store, Some(root)).await;
        for id in [root, child] {
            let code = refusal(
                resolve_launch_exec_target(
                    &store,
                    &runtime("ssh://arch-laptop"),
                    id,
                    None,
                    &ExecTarget::Host,
                )
                .await,
            )
            .await;
            assert_eq!(code, "exec_target_unavailable:binding_unreadable", "{id}");
        }
    }
}
