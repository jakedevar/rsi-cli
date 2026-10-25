//! The case map of the legacy `scripts/tests/test_target_pool.py` (75 cases, #1741, #1882,
//! #2707, #2717, #2773), deleted with the Python tool it tested (#453, #446, #494).
//!
//! Every legacy case is mapped below to the Rust test(s) that now carry it, or retired with a
//! reason. [`CASES`] is that table as data, and `the_case_map_*` tests keep it honest: all 75
//! legacy names appear once, none maps to nothing, and every Rust test it names exists as a
//! `fn` in one of the suites (so renaming or deleting a mapped test fails here).
//!
//! The suites, by what they run against:
//!
//! - `target_pool_tests.rs` (`seed`, `key`): real Git, a stub `rustc`.
//! - `target_pool_ops_tests.rs` (`gc`, `reclaim-idle`, `refresh`, `status`, `summary`, disk
//!   floors, admission): real Git, fabricated pool directories, the disk the code sees stated
//!   per test (`test_disk`).
//! - `target_pool_build_tests.rs` (`build`, the detached request, `seed-from`): real Git, a stub
//!   `cargo` that fabricates the target tree, a local stand-in for ssh.
//! - `crates/rsid/tests/target_pool_cargo.rs` (the real-Cargo cases): a tiny offline workspace
//!   built by real `cargo`, with and without the `rsi-build-rustc` this package builds. It is
//!   in `rsid` because only that package's integration tests can name the wrapper binary.
//! - `scripts/tests/test_target_pool_shim.sh`: the `scripts/target-pool` shim every caller
//!   (cargo-slot, the daemon, the Makefile) now goes through.
//!
//! What changed on purpose, so a reader of the table knows what "ported" means:
//!
//! - Where the Python suite patched a function (`mock.patch.dict(LIVE, ...)`), the Rust tool has
//!   an explicit seam instead: `gc_with` (a call just before gc locks a doomed pool),
//!   `test_disk` (the free space), and an injected `Launch` for refresh. No production behaviour
//!   differs under test.
//! - The legacy `seed-from` cases ran `du -sb` on the local ssh stand-in and failed on macOS
//!   (#494). The peer's size is now `du -sk` (GNU and BSD share it), and the stand-in `du` in
//!   `target_pool_build_tests.rs` refuses every other flag, like BSD `du`, so the old probe
//!   cannot come back unnoticed.
//! - Cases whose assertion needs a tool the host may lack skip with a printed reason: `zstd` for
//!   `seed-from`, `sccache` for the sccache start-up case. A skipped case executes nothing and
//!   is never evidence; the hub Linux gate (which has both) is the acceptance route for them.
//!
//! Negative and mutation proofs for the risky cases (each fails if the named code is reverted):
//!
//! - gc lock identity (legacy 26): the test keeps a descriptor on the lock file across another
//!   gc's removal and a rebuild, then needs it to conflict with the seed's lock; it also builds
//!   the opposite world (the lock file unlinked and recreated) and needs the same probe to say
//!   "no conflict", so the probe provably separates the two.
//! - gc plan outliving a rebuild (27): `gc_with` rebuilds the pool between plan and lock; reverting
//!   the generation check (`same_generation`) removes the new pool and fails the test.
//! - the portable size probe (8, 9): the peer's `du` stand-in exits 64 on any flag but `-sk`.
//! - the shim (shell suite): a shim that dropped `target-pool` from the exec line, ignored the
//!   exit code, preferred PATH to the sibling binary or ran a non-executable file fails a named
//!   case.
//! - clobber (19-21): the pool's files are hashed before and after two sandboxes build on top of
//!   it; the fail-closed case needs the build to *stop* without the wrapper and leave the hashes.

/// One legacy case: its name, and the Rust tests that carry it (each a `fn` in a suite named in
/// the header; `rsid:` marks `crates/rsid/tests/target_pool_cargo.rs`).
pub(crate) struct Case {
    pub legacy: &'static str,
    pub carried_by: &'static [&'static str],
    /// How the Rust version differs from the legacy case, when it does.
    pub note: &'static str,
}

const fn case(
    legacy: &'static str,
    carried_by: &'static [&'static str],
    note: &'static str,
) -> Case {
    Case {
        legacy,
        carried_by,
        note,
    }
}

pub(crate) const CASES: [Case; 75] = [
    // ---- class TargetPool ----
    case(
        "test_same_commit_seed_is_fresh_and_relocates_baked_paths",
        &["rsid:same_commit_seed_is_fresh_and_relocates_baked_paths"],
        "real Cargo",
    ),
    case(
        "test_pool_build_reuses_ancestor_and_preserves_its_artifacts",
        &[
            "rsid:pool_build_reuses_ancestor_and_preserves_its_artifacts",
            "a_descendant_build_is_seeded_from_the_nearest_ready_ancestor_by_hard_links",
        ],
        "real Cargo, plus the stub-cargo marker/link fields",
    ),
    case(
        "test_pool_build_reuses_shared_executables_with_wrapper",
        &[
            "rsid:pool_build_reuses_shared_executables_with_wrapper",
            "the_wrapper_that_unlinks_outputs_lets_executables_be_linked",
        ],
        "the wrapper is this package's own rsi-build-rustc, never skipped",
    ),
    case(
        "test_pool_build_chooses_nearest_ready_ancestor",
        &[
            "rsid:pool_build_chooses_nearest_ready_ancestor",
            "a_sandbox_is_seeded_from_the_nearest_ancestor_pool_and_only_what_differs_rebuilds",
        ],
        "",
    ),
    case(
        "test_pool_build_is_cold_without_a_compatible_nearby_ancestor",
        &["rsid:pool_build_is_cold_without_a_compatible_nearby_ancestor"],
        "both sub-cases (max distance 0; another build key)",
    ),
    case(
        "test_pool_build_is_cold_when_ready_pool_is_a_descendant",
        &["rsid:pool_build_is_cold_when_ready_pool_is_a_descendant"],
        "",
    ),
    case(
        "test_key_mismatch_and_non_empty_target_are_not_seeded",
        &[
            "a_key_mismatch_a_non_empty_target_and_a_non_workspace_are_not_seeded",
            "no_ready_pool_with_this_key_or_ancestry_is_a_named_reason_not_an_error",
        ],
        "",
    ),
    case(
        "test_seed_from_another_host_copies_the_newest_matching_pool",
        &["seed_from_another_host_copies_the_newest_matching_pool"],
        "portable du (#494); skips without zstd",
    ),
    case(
        "test_gc_racing_a_seed_from_cannot_remove_the_half_copied_pool",
        &["gc_racing_a_seed_from_cannot_remove_the_half_copied_pool"],
        "the ssh stand-in parks the transfer while the test runs gc; skips without zstd",
    ),
    case(
        "test_pool_floor_is_capped_at_a_quarter_of_the_disk",
        &["the_pool_floor_is_capped_at_a_quarter_of_the_disk"],
        "",
    ),
    case(
        "test_seed_keys_and_requests_the_sandbox_head_despite_inherited_git_context",
        &[
            "seed_keys_and_requests_the_sandbox_head_despite_inherited_git_context",
            "the_pool_build_environment_drops_launch_scratch_and_keeps_cache_settings",
        ],
        "the detached build's argv is read from a stub program",
    ),
    case(
        "test_pool_unit_drops_launch_scratch_and_keeps_cache_settings",
        &[
            "a_build_request_runs_the_detached_unit_in_the_slice_with_only_the_allowed_environment",
            "the_pool_build_environment_drops_launch_scratch_and_keeps_cache_settings",
        ],
        "",
    ),
    case(
        "test_pool_unit_limits_follow_the_slice_budget_and_settings",
        &[
            "pool_unit_limits_follow_the_slice_budget_and_settings",
            "the_quota_text_parses_like_the_python_regex",
        ],
        "",
    ),
    case(
        "test_pool_unit_without_systemd_runs_niced",
        &["a_build_request_without_systemd_starts_a_detached_niced_process"],
        "",
    ),
    case(
        "test_pool_environment_starts_sccache_through_wrapper_with_long_pool_path",
        &[
            "rsid:pool_environment_starts_sccache_through_wrapper_with_long_pool_path",
            "the_pool_build_environment_drops_launch_scratch_and_keeps_cache_settings",
        ],
        "real sccache; skips without it (as the legacy case did)",
    ),
    case(
        "test_failed_build_diagnostics_are_visible_in_status_and_next_seed",
        &[
            "failed_build_diagnostics_are_visible_in_status_and_next_seed",
            "a_real_build_break_fails_the_pool_with_its_diagnostics_in_the_marker",
        ],
        "exit code 9 through a failing cargo-slot stand-in",
    ),
    case(
        "test_a_tip_whose_tests_do_not_compile_still_gets_a_pool",
        &["a_tip_whose_tests_do_not_compile_still_gets_a_degraded_pool"],
        "stub cargo",
    ),
    case(
        "test_a_real_build_break_still_fails_the_pool",
        &["a_real_build_break_fails_the_pool_with_its_diagnostics_in_the_marker"],
        "stub cargo",
    ),
    case(
        "test_clobber_two_sandboxes_on_different_commits_build_concurrently",
        &["rsid:clobber_two_sandboxes_on_different_commits_build_concurrently"],
        "real Cargo, two concurrent builds",
    ),
    case(
        "test_clobber_with_shared_executables_behind_the_wrapper",
        &["rsid:clobber_with_shared_executables_behind_the_wrapper"],
        "",
    ),
    case(
        "test_shared_executables_fail_closed_without_the_wrapper",
        &["rsid:shared_executables_fail_closed_without_the_wrapper"],
        "",
    ),
    // ---- class PoolLifecycle ----
    case(
        "test_gc_keeps_the_newest_n_ready_pools",
        &[
            "gc_keeps_the_newest_n_ready_pools",
            "gc_removes_what_the_plan_dooms_and_keeps_the_marker_and_log_rules",
        ],
        "",
    ),
    case(
        "test_gc_pressure_keeps_only_the_newest_pool",
        &["gc_pressure_keeps_only_the_newest_pool"],
        "",
    ),
    case(
        "test_gc_keeps_the_newest_complete_pool_behind_degraded_ones",
        &["gc_keeps_the_newest_complete_pool_behind_degraded_ones_and_under_pressure_too"],
        "",
    ),
    case(
        "test_gc_pressure_keeps_the_last_complete_pool_under_a_newer_degraded_one",
        &["gc_pressure_keeps_the_last_complete_pool_under_a_newer_degraded_one"],
        "",
    ),
    case(
        "test_a_stale_gc_cannot_delete_a_pool_rebuilt_and_seeded_meanwhile",
        &["a_stale_gc_cannot_delete_a_pool_rebuilt_and_seeded_meanwhile"],
        "lock identity checked on a real descriptor, with a control",
    ),
    case(
        "test_a_stale_gc_does_not_remove_a_pool_rebuilt_since_its_plan",
        &[
            "a_stale_gc_does_not_remove_a_pool_rebuilt_since_its_plan",
            "a_stale_gc_cannot_delete_a_pool_rebuilt_since_its_plan",
        ],
        "gc_with injects the rebuild",
    ),
    case(
        "test_gc_ignores_failed_markers_and_counts_only_ready_pools",
        &["gc_ignores_failed_markers_and_counts_only_ready_pools"],
        "",
    ),
    case(
        "test_gc_never_removes_a_pool_a_seed_is_reading",
        &["gc_never_removes_a_pool_a_seed_is_reading_and_names_it"],
        "",
    ),
    case(
        "test_a_pool_gc_is_removing_is_not_offered_to_a_seed",
        &[
            "a_pool_gc_is_removing_is_not_offered_to_a_seed",
            "a_pool_gc_holds_exclusively_is_skipped",
        ],
        "",
    ),
    case(
        "test_keep_comes_from_the_operator_setting",
        &["the_keep_count_comes_from_the_operator_setting"],
        "",
    ),
    case(
        "test_gc_command_reports_removed_and_in_use_pools_as_json",
        &["the_gc_command_reports_removed_and_in_use_pools_as_json_and_as_lines"],
        "",
    ),
    case(
        "test_a_stale_pool_is_doomed_even_when_it_is_the_newest_and_the_only_one",
        &[
            "a_stale_pool_is_doomed_even_when_it_is_the_newest_and_the_only_one_and_does_not_use_up_the_keep_count",
        ],
        "one test carries both stale-plan cases",
    ),
    case(
        "test_a_stale_pool_does_not_use_up_the_keep_count",
        &[
            "a_stale_pool_is_doomed_even_when_it_is_the_newest_and_the_only_one_and_does_not_use_up_the_keep_count",
        ],
        "same test as the case above",
    ),
    case(
        "test_gc_collects_pools_beyond_the_seed_distance_of_the_tip",
        &["gc_collects_pools_beyond_the_seed_distance_of_the_tip_and_keeps_near_or_unknown_ones"],
        "",
    ),
    case(
        "test_gc_keeps_a_pool_within_the_limit_and_one_whose_age_is_unknown",
        &["gc_collects_pools_beyond_the_seed_distance_of_the_tip_and_keeps_near_or_unknown_ones"],
        "same test as the case above",
    ),
    case(
        "test_gc_never_removes_a_stale_pool_a_seed_is_reading",
        &["gc_never_removes_a_stale_pool_a_seed_is_reading"],
        "",
    ),
    case(
        "test_gc_command_collects_a_stale_pool_unless_the_ref_is_emptied",
        &["the_gc_command_collects_a_stale_pool_unless_the_ref_is_emptied"],
        "through the command line parser, including its usage error",
    ),
    case(
        "test_the_nearest_pool_triggers_a_refresh_past_half_the_distance_limit",
        &["the_nearest_pool_triggers_a_refresh_past_half_the_distance_limit"],
        "",
    ),
    case(
        "test_gc_explicit_zero_evicts_the_last_complete_pool_only_after_its_reader_finishes",
        &[
            "an_explicit_zero_evicts_the_last_complete_pool_only_after_its_reader_finishes_under_pressure_too",
            "gc_zero_evicts_the_last_complete_pool_only_after_its_reader_finishes",
        ],
        "with and without --pressure, another repo's directory untouched",
    ),
    case(
        "test_summary_names_the_newest_good_pool_its_size_and_a_newer_failure",
        &["summary_names_the_newest_good_pool_its_size_and_a_newer_failure"],
        "",
    ),
    case(
        "test_summary_counts_only_idle_pools_on_the_requested_filesystem",
        &[
            "summary_counts_only_idle_pools_on_the_requested_filesystem_and_sizes_an_old_pool_once",
            "summary_and_gc_leave_a_pool_alone_on_a_filesystem_they_were_not_asked_about",
        ],
        "the other filesystem is a path that is not one",
    ),
    case(
        "test_summary_sizes_an_old_pool_once_and_remembers_it",
        &["summary_counts_only_idle_pools_on_the_requested_filesystem_and_sizes_an_old_pool_once"],
        "",
    ),
    case(
        "test_summary_cannot_resurrect_a_marker_gc_removed_during_the_size_walk",
        &[
            "pool_size_cannot_resurrect_a_marker_removed_during_the_walk",
            "pool_size_yields_while_gc_holds_the_pool_and_writes_nothing",
        ],
        "the walk holds the shared use lock; both halves are asserted",
    ),
    case(
        "test_size_cache_does_not_rewrite_a_marker_that_was_removed",
        &["pool_size_cannot_resurrect_a_marker_removed_during_the_walk"],
        "",
    ),
    case(
        "test_a_ready_marker_without_a_target_is_not_ready_and_not_counted",
        &["a_ready_marker_without_a_target_is_not_ready_and_not_counted"],
        "",
    ),
    // ---- class NoPoolReason ----
    case(
        "test_no_ready_pool_for_the_key_is_named",
        &["no_ready_pool_with_this_key_or_ancestry_is_a_named_reason_not_an_error"],
        "",
    ),
    case(
        "test_a_pool_that_is_not_an_ancestor_is_named",
        &[
            "no_ready_pool_with_this_key_or_ancestry_is_a_named_reason_not_an_error",
            "the_nearest_ancestor_wins_and_an_exact_pool_is_distance_zero",
        ],
        "",
    ),
    case(
        "test_an_ancestor_beyond_the_limit_is_refused_and_its_distance_reported",
        &[
            "a_pool_that_is_too_far_behind_or_gone_from_disk_is_not_offered",
            "seed_reports_the_nearest_pool_next_to_no_pool",
        ],
        "",
    ),
    case(
        "test_the_nearest_ancestor_wins_and_an_exact_pool_is_distance_zero",
        &["the_nearest_ancestor_wins_and_an_exact_pool_is_distance_zero"],
        "",
    ),
    case(
        "test_seed_reports_the_reason_next_to_no_pool",
        &[
            "seed_reports_the_nearest_pool_next_to_no_pool",
            "seed_without_a_pool_within_the_distance_limit_requests_a_build",
        ],
        "",
    ),
    case(
        "test_seed_nearby_pool_skips_one_off_build_and_records_reason",
        &["seed_with_a_nearby_pool_skips_the_one_off_build_and_records_the_reason"],
        "",
    ),
    case(
        "test_seed_without_a_pool_within_max_distance_requests_build",
        &["seed_without_a_pool_within_the_distance_limit_requests_a_build"],
        "",
    ),
    case(
        "test_seed_at_rolling_tip_with_nearby_pool_requests_build",
        &["seed_at_the_rolling_tip_with_a_nearby_pool_requests_a_build"],
        "",
    ),
    case(
        "test_seed_without_origin_rolling_uses_nearby_pool_and_records_skip",
        &["seed_without_origin_rolling_uses_the_nearby_pool_and_records_the_skip"],
        "",
    ),
    case(
        "test_seed_at_rolling_tip_with_exact_pool_uses_it",
        &["seed_at_the_rolling_tip_with_an_exact_pool_uses_it_and_builds_nothing"],
        "",
    ),
    case(
        "test_stale_pools_are_those_more_than_the_limit_behind_the_tip",
        &[
            "gc_collects_pools_beyond_the_seed_distance_of_the_tip_and_keeps_near_or_unknown_ones",
            "stale_pool_names_are_empty_for_a_ref_that_does_not_resolve",
        ],
        "",
    ),
    case(
        "test_a_pool_directory_remembers_its_checkout_before_the_last_pool_goes",
        &["a_pool_directory_remembers_its_checkout_before_the_last_pool_goes"],
        "",
    ),
    case(
        "test_refresh_builds_at_the_tip_when_the_only_pool_was_collected_as_stale",
        &["refresh_builds_at_the_tip_when_the_only_pool_was_collected_as_stale"],
        "",
    ),
    case(
        "test_an_imported_pool_is_aged_through_the_checkout_its_directory_belongs_to",
        &["an_imported_pool_is_aged_through_the_checkout_its_directory_belongs_to"],
        "",
    ),
    case(
        "test_seed_from_records_the_checkout_of_the_pool_it_imports",
        &["seed_from_records_the_checkout_of_the_pool_it_imports"],
        "through a real seed-from over the ssh stand-in, not a patched stream",
    ),
    case(
        "test_a_stale_imported_pool_is_collected_then_refreshed_without_a_repo",
        &["a_stale_imported_pool_is_collected_then_refreshed_without_a_repo"],
        "",
    ),
    case(
        "test_gc_with_a_repo_records_the_checkout_of_an_imported_pool_directory",
        &["gc_with_a_repo_records_the_checkout_of_an_imported_pool_directory"],
        "",
    ),
    case(
        "test_refresh_leaves_a_fresh_pool_and_a_recent_failure_alone",
        &["refresh_leaves_a_fresh_pool_and_a_recent_failure_alone"],
        "",
    ),
    case(
        "test_refresh_without_a_known_checkout_or_ref_says_so_and_builds_nothing",
        &["refresh_without_a_known_checkout_or_ref_says_so_and_builds_nothing"],
        "",
    ),
    case(
        "test_a_build_request_below_the_free_space_floor_says_so_and_starts_nothing",
        &["a_build_request_below_the_free_space_floor_says_so_and_starts_nothing"],
        "",
    ),
    // ---- class PoolDiskFloor ----
    case(
        "test_expected_size_is_the_nearest_ancestor_then_the_newest_pool_then_the_last_then_default",
        &[
            "the_expected_size_is_the_nearest_ancestor_then_the_newest_pool_then_the_last_then_the_default",
        ],
        "",
    ),
    case(
        "test_admission_needs_the_floor_plus_the_expected_pool_size",
        &["admission_needs_the_floor_plus_the_expected_pool_size"],
        "",
    ),
    case(
        "test_the_workers_floor_counts_when_it_is_above_the_pool_floor",
        &[
            "the_workers_floor_counts_when_it_is_above_the_pool_floor_on_any_disk",
            "the_worker_floor_comes_from_the_daemon_setting_then_cargo_slot_then_thirty",
        ],
        "floors stated as fractions of the host disk, so no disk size makes it a no-op",
    ),
    case(
        "test_a_skipped_request_records_the_reason_for_status_and_a_started_one_clears_it",
        &[
            "a_skipped_request_is_history_once_a_newer_pool_exists",
            "a_build_below_the_free_space_floor_is_skipped_with_the_typed_reason",
            "events_are_remembered_per_kind_and_forgotten_by_none",
        ],
        "",
    ),
    case(
        "test_a_guarded_build_is_stopped_the_moment_free_space_crosses_the_floor",
        &[
            "a_guarded_build_is_stopped_the_moment_free_space_crosses_the_floor_and_leaves_no_partial_pool",
        ],
        "",
    ),
    case(
        "test_an_aborted_pool_build_removes_its_partial_output_and_reports_the_reason",
        &[
            "a_guarded_build_is_stopped_the_moment_free_space_crosses_the_floor_and_leaves_no_partial_pool",
        ],
        "same test as the case above",
    ),
    case(
        "test_reclaim_idle_evicts_unleased_pools_skips_a_leased_one_and_is_bounded",
        &["reclaim_idle_evicts_unleased_pools_skips_a_leased_one_and_is_bounded"],
        "",
    ),
    case(
        "test_the_last_pool_size_survives_a_reclaim_for_the_next_admission",
        &["the_last_pool_size_survives_a_reclaim_for_the_next_admission"],
        "",
    ),
    case(
        "test_seed_from_leaves_the_workers_their_floor_too",
        &["seed_from_leaves_the_workers_their_floor_too"],
        "skips on a disk too small to state the worker floor",
    ),
];

/// The suites a case may name, by the file that holds its `fn`: the three in this crate, then
/// the real-Cargo file in `rsid` (read at run time: it is another package's file).
#[cfg(test)]
fn suites() -> Vec<String> {
    let cargo = std::fs::read_to_string(
        std::path::Path::new(rsi_common::manifest_dir!())
            .join("../rsid/tests/target_pool_cargo.rs"),
    )
    .expect("crates/rsid/tests/target_pool_cargo.rs");
    vec![
        include_str!("target_pool_tests.rs").to_string(),
        include_str!("target_pool_ops_tests.rs").to_string(),
        include_str!("target_pool_build_tests.rs").to_string(),
        cargo,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn defines(suites: &[String], name: &str) -> bool {
        let needle = format!("fn {name}(");
        suites.iter().any(|suite| suite.contains(&needle))
    }

    #[test]
    fn the_case_map_has_every_one_of_the_75_legacy_cases_once() {
        assert_eq!(CASES.len(), 75);
        let names: BTreeSet<&str> = CASES.iter().map(|c| c.legacy).collect();
        assert_eq!(names.len(), 75, "a legacy case is listed twice");
        let per_class = |first: &str, last: &str| {
            let from = CASES.iter().position(|c| c.legacy == first).unwrap();
            let to = CASES.iter().position(|c| c.legacy == last).unwrap();
            to - from + 1
        };
        assert_eq!(
            per_class(
                "test_same_commit_seed_is_fresh_and_relocates_baked_paths",
                "test_shared_executables_fail_closed_without_the_wrapper"
            ),
            21
        );
        assert_eq!(
            per_class(
                "test_gc_keeps_the_newest_n_ready_pools",
                "test_a_ready_marker_without_a_target_is_not_ready_and_not_counted"
            ),
            25
        );
        assert_eq!(
            per_class(
                "test_no_ready_pool_for_the_key_is_named",
                "test_a_build_request_below_the_free_space_floor_says_so_and_starts_nothing"
            ),
            20
        );
        assert_eq!(
            per_class(
                "test_expected_size_is_the_nearest_ancestor_then_the_newest_pool_then_the_last_then_default",
                "test_seed_from_leaves_the_workers_their_floor_too"
            ),
            9
        );
    }

    #[test]
    fn the_case_map_names_only_tests_that_exist() {
        let suites = suites();
        let mut missing = vec![];
        for case in &CASES {
            assert!(
                !case.carried_by.is_empty(),
                "{} maps to nothing (port note: {})",
                case.legacy,
                case.note
            );
            for carried in case.carried_by {
                let name = carried.strip_prefix("rsid:").unwrap_or(carried);
                if !defines(&suites, name) {
                    missing.push(format!(
                        "{} -> {carried} (port note: {})",
                        case.legacy, case.note
                    ));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "mapped tests that do not exist: {missing:#?}"
        );
    }

    #[test]
    fn the_case_map_check_reports_a_test_nobody_wrote() {
        // The control: a name no suite defines is not found, so the check above can fail.
        let suites = suites();
        assert!(!defines(&suites, "a_test_nobody_wrote"));
        assert!(defines(&suites, "gc_keeps_the_newest_n_ready_pools"));
    }

    #[test]
    fn every_real_cargo_test_is_in_the_map() {
        let cargo = suites().pop().unwrap();
        let lines: Vec<&str> = cargo.lines().collect();
        let mut unmapped = vec![];
        for pair in lines.windows(2) {
            if pair[0].trim() == "#[test]" {
                let name = pair[1].trim_start_matches("fn ").split('(').next().unwrap();
                let tag = format!("rsid:{name}");
                if !CASES.iter().any(|c| c.carried_by.contains(&tag.as_str())) {
                    unmapped.push(name.to_string());
                }
            }
        }
        assert!(
            unmapped.is_empty(),
            "real-Cargo tests no legacy case maps to: {unmapped:?}"
        );
    }
}

#[cfg(test)]
mod key_env_pin {
    use crate::target_pool::{KEY_ENV_EXACT, KEY_ENV_PREFIX};

    /// `rsid-store` cannot link this crate (its `serde_json` feature set changes the topology
    /// starters' canonical digests in `rsid`'s tests), so its test of "the refresh and the
    /// worker seed yield one pool key" compares the environment the key depends on, from a copy
    /// of this list. This pins the copy.
    #[test]
    fn the_key_environment_list_copied_into_rsid_store_matches_the_tool() {
        let copy = std::fs::read_to_string(
            std::path::Path::new(rsi_common::manifest_dir!())
                .join("../rsid-store/src/sandbox/execution_scratch.rs"),
        )
        .unwrap();
        for name in KEY_ENV_EXACT {
            assert!(
                copy.contains(&format!("\"{name}\",")),
                "{name} is missing from the copy"
            );
        }
        assert!(copy.contains(&format!("starts_with(\"{KEY_ENV_PREFIX}\")")));
        assert!(copy.contains(&format!("const EXACT: [&str; {}]", KEY_ENV_EXACT.len())));
    }
}
