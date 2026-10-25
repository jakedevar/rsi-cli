//! Shared Store mutex instrumentation.

pub(crate) use rsid_core::store_lock::*;

#[cfg(test)]
mod tests {
    /// #3021: the manager coordinator (10 s) and the manager action backstop
    /// (10 s) hold the shared Store through these files. An unlabelled holder
    /// shows as `unlabelled` in a lock-wait warning and hides which pass
    /// kept the Store; every acquire in them names itself.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn periodic_manager_paths_label_every_store_acquire() {
        let files: [(&str, &str); 8] = [
            (
                "manager_actions",
                include_str!("session/manager_actions.rs"),
            ),
            (
                "manager_actions/recovery",
                include_str!("session/manager_actions/recovery.rs"),
            ),
            (
                "manager_coordinator",
                include_str!("session/manager_coordinator.rs"),
            ),
            (
                "manager_issue_worker",
                include_str!("session/manager_issue_worker.rs"),
            ),
            (
                "manager_succession",
                include_str!("session/manager_succession.rs"),
            ),
            ("manager_reseat", include_str!("session/manager_reseat.rs")),
            ("remote_answers", include_str!("session/remote_answers.rs")),
            (
                "pending_approvals",
                include_str!("session/pending_approvals.rs"),
            ),
        ];
        let mut unlabelled = Vec::new();
        for (name, source) in files {
            let squeezed: String = source.chars().filter(|c| !c.is_whitespace()).collect();
            for needle in [
                "store.lock().await",
                "store.try_lock()",
                "store.blocking_lock()",
            ] {
                let count = squeezed.matches(needle).count();
                if count > 0 {
                    unlabelled.push(format!("{name}: {count} x {needle}"));
                }
            }
        }
        assert!(
            unlabelled.is_empty(),
            "use crate::store_lock::lock(&store, \"<module>::<fn>\"): {unlabelled:?}"
        );
        assert!(
            include_str!("session/lifecycle.rs")
                .contains("\"lifecycle::check_manager_notice_resume\""),
            "the manager notice-resume Store acquire lost its label"
        );
    }
}
