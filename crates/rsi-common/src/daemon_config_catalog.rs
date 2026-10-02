//! Static catalog of every persisted daemon runtime-config field: when a
//! change applies, and which operator surface edits it.
//!
//! Both sides check it (neither crate can see the other): the rsid test
//! `config::tests::persisted_fields_are_catalogued` requires one entry per
//! `PERSISTED_RUNTIME_CONFIG_FIELDS` name, and the rsi test
//! `settings_registry::tests::every_daemon_catalog_field_has_one_surface`
//! requires every `SettingsPage` entry to have exactly one settings row. A new
//! persisted daemon setting therefore cannot ship without a catalog entry and
//! an operator surface (AGENTS.md "Database Rules", issue #35).
//!
//! Apply classes are hand-declared from the daemon code (cited per entry).
//! When liveness could not be established the class is `DaemonRestart`, the
//! safe direction: the badge may over-warn but never under-warns.

/// When a persisted daemon field's new value takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyClass {
    /// Read at use time; applies immediately.
    Live,
    /// Read when a provider process is spawned.
    NextSpawn,
    /// Applies to new launches now and to resumed sessions after a restart.
    PartialLive,
    /// Turning it off applies now; turning it on needs a daemon restart.
    LiveOffRestartOn,
    /// Read once at daemon boot.
    DaemonRestart,
    /// Persisted, but the daemon does not apply it to the behavior its name
    /// suggests; the row's summary says what actually happens.
    NotApplied,
}

impl ApplyClass {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "immediately",
            Self::NextSpawn => "next spawn",
            Self::PartialLive => "new launches now · resumed sessions after restart",
            Self::LiveOffRestartOn => "off now · on after restart",
            Self::DaemonRestart => "after daemon restart",
            Self::NotApplied => "stored, not applied (see summary)",
        }
    }
}

/// Where the operator edits a persisted daemon field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorSurface {
    /// A row of the TUI settings page.
    SettingsPage,
    /// Another named TUI surface, proven to write the field: `writer` is the
    /// `crates/rsi/src`-relative file holding its `UpdateDaemonConfig` write
    /// (checked by `settings_registry::tests`).
    Elsewhere {
        surface: &'static str,
        writer: &'static str,
    },
    /// No TUI editor yet (issue #35 / #675 gap). `set_via` says how the
    /// operator sets it today; the manual lists every such field.
    NoTuiEditor {
        set_via: &'static str,
        tracking: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonFieldSpec {
    pub field: &'static str,
    pub apply: ApplyClass,
    pub operator_surface: OperatorSurface,
}

const fn page(field: &'static str, apply: ApplyClass) -> DaemonFieldSpec {
    DaemonFieldSpec {
        field,
        apply,
        operator_surface: OperatorSurface::SettingsPage,
    }
}

/// Builds a `NoTuiEditor` entry. Currently unused (slice (c) wired the last
/// two gap fields, `gv_render_recursive_origin`/`gv_info_dashboard`, onto
/// real settings rows) but kept for the next Issue #35 gap, so a future entry
/// doesn't have to reintroduce this constructor from scratch.
#[allow(dead_code)]
const fn no_tui_editor(
    field: &'static str,
    apply: ApplyClass,
    set_via: &'static str,
    tracking: &'static str,
) -> DaemonFieldSpec {
    DaemonFieldSpec {
        field,
        apply,
        operator_surface: OperatorSurface::NoTuiEditor { set_via, tracking },
    }
}

use ApplyClass::{DaemonRestart, LiveOffRestartOn, NextSpawn, PartialLive};
const LIVE: ApplyClass = ApplyClass::Live;

/// One entry per persisted daemon runtime-config field, in the order of
/// `rsid::config::PERSISTED_RUNTIME_CONFIG_FIELDS`.
pub static DAEMON_CONFIG_FIELDS: &[DaemonFieldSpec] = &[
    page("session_retention_enabled", LIVE),
    page("session_retention_window_hours", LIVE),
    // S-038, verify-settings-page.md.
    page("retry_enabled", LIVE),
    // #925: the hub poller reads the runtime atomic on every registry tick
    // (rsid satellite/hub.rs `run_hub_poller`), so a flip takes effect without
    // a daemon restart.
    page("satellite_polling_enabled", LIVE),
    // Not applied: rsid session/retry_policy.rs:14-18 `kind_default_max_retries`
    // returns 0 for every SessionKind, and `effective_default` (:28-34) reads
    // this field and discards it. Default launches get zero automatic retries
    // unless the launch carries an explicit retry policy (fail-closed; kept).
    page("retry_max_default", ApplyClass::NotApplied),
    // S-040: the stall-retry handler is spawned at boot.
    page("retry_on_stall", DaemonRestart),
    // rsid session/lifecycle.rs:4206 loads the atomic per backoff.
    page("retry_max_backoff_ms", LIVE),
    // S-041.
    page("reconciliation_enabled", DaemonRestart),
    // S-042.
    page("stall_detection_enabled", DaemonRestart),
    // S-043.
    page("context_rotation_enabled", PartialLive),
    // Read from RuntimeConfig on every context usage update, including active sessions.
    page("context_rotation_global_pct", LIVE),
    page("context_rotation_claude_pct", LIVE),
    page("context_rotation_codex_pct", LIVE),
    page("completed_transcript_cache_max_bytes", LIVE),
    // S-044: OFF is read per memory operation (rsid memory/worker.rs
    // `MemoryWorkContext::check`, `ObservationSubmission::submit`); the worker
    // is only started at boot when enabled, so ON needs a restart.
    page("memory_enabled", LiveOffRestartOn),
    // Epic L CG-S3: the indexer shares the runtime `Arc<AtomicBool>` (rsid
    // codegraph/runtime.rs:482) and reads it per indexing pass.
    page("codegraph_indexing_enabled", LIVE),
    // S-045.
    page("queue_enabled", DaemonRestart),
    // S-046.
    page("dream_enabled", LIVE),
    // S-047.
    page("dialectic_enabled", DaemonRestart),
    // Title model: rsid session/launch.rs:7245-7248 and
    // session/lifecycle.rs:1322-1325 read these per title request.
    page("title_model_local", LIVE),
    page("title_model_provider", LIVE),
    page("title_model_base_url", LIVE),
    page("title_model_fallback", LIVE),
    // Memory models: rsid observation/extractor.rs:214-228 and
    // session/summarizer.rs:229-242 read these per extraction.
    page("memory_model_local", LIVE),
    page("memory_model_fallback", LIVE),
    page("memory_model_fallback_provider", LIVE),
    page("memory_model_fallback_base_url", LIVE),
    // Dream model: rsid dreamer/scheduler.rs:563-565 reads these per cycle.
    page("dream_model", LIVE),
    page("dream_model_provider", LIVE),
    page("dream_model_base_url", LIVE),
    // S-031: DreamConfig is built at boot (rsid main.rs:662-665).
    page("dream_observation_threshold", DaemonRestart),
    // rsid dreamer/scheduler.rs:593 loads the atomic per cycle.
    page("dream_idle_secs", LIVE),
    // S-032.
    page("dream_cooldown_secs", DaemonRestart),
    // Prompt compiler: rsid prompt_compile/engine.rs:102-111 reads per compile.
    page("prompt_compile_model_local", LIVE),
    page("prompt_compile_model_provider", LIVE),
    page("prompt_compile_model_base_url", LIVE),
    // S-048 (rsid codex.rs:342).
    page("codex_sandbox_mode", NextSpawn),
    // S-049 (rsid claude.rs:484).
    page("claude_config_isolation", NextSpawn),
    // Unverified: no read site outside config.rs was found; safe default.
    page("system_prompt_preset", DaemonRestart),
    // S-062.
    page("stall_classifier_enabled", LiveOffRestartOn),
    // S-025: the classifier is built at boot (rsid config.rs:1218, main.rs:226).
    page("stall_classifier_model", DaemonRestart),
    // S-064 .. S-067.
    page("stall_classifier_idle_secs", DaemonRestart),
    page("stall_classifier_idle_secs_codex", DaemonRestart),
    page("stall_classifier_cooldown_secs", DaemonRestart),
    page("stall_classifier_max_per_session", DaemonRestart),
    // S-068.
    page("stall_classifier_confidence_floor", LIVE),
    // Recursive DAG controls: rsid rpc.rs:5013-5397 and 7224-7292 read the
    // runtime config per request.
    page("recursive_dag_recovery_controls_enabled", LIVE),
    page("recursive_dag_scheduler_controls_enabled", LIVE),
    page("recursive_dag_cancellation_controls_enabled", LIVE),
    page("recursive_dag_live_scheduler_control_enabled", LIVE),
    page("recursive_dag_run_lease_ttl_ms", LIVE),
    page("recursive_dag_max_concurrent_graphs", LIVE),
    // Graph view capabilities: rsid rpc.rs:7254 reads them per capability
    // request. The graph overlay only reads them, but slice (c) adds a real
    // settings row (AGENT AUTOMATION > Orchestration) that writes them via
    // `update_daemon_config` (confirmed validating `RuntimeConfig::update_field`
    // arms at rsid config.rs:1148 and :1153).
    page("gv_render_recursive_origin", LIVE),
    page("gv_info_dashboard", LIVE),
    // #634 durable topology executor kill switch: rsid
    // session/graph_executions.rs:394-396 reads the atomic per decision.
    page("topology_executor_enabled", LIVE),
    // rsid session/graph_executions.rs build_node_cap() loads the atomic
    // per scheduling decision.
    page("topology_max_concurrent_build_nodes", LIVE),
    // T4: read when topology fan-out is planned.
    page("topology_bulk_fanout_min_openrouter", LIVE),
    // rsid store/model_control.rs:4678 reads the ceiling per admission.
    page("orchestration_max_child_effort", LIVE),
    // Issue #692: read per launch by rsid session/launch.rs (the single launch
    // chokepoint; the effective model, defaults applied, is checked before any
    // side effect), the continuation/rotation preflights and the AgentSpawnChild
    // pre-check. Edited by the TUI command `:launch-allow`, which writes it
    // through `update_daemon_config`.
    DaemonFieldSpec {
        field: crate::launch_allowlist::LAUNCH_MODEL_ALLOWLIST_FIELD,
        apply: LIVE,
        operator_surface: OperatorSurface::Elsewhere {
            surface: ":launch-allow command",
            writer: "action_handler/daemon_config.rs",
        },
    },
    // S-052, S-054 .. S-058.
    page("sandbox_build_cache_reclaim_enabled", LIVE),
    page("sandbox_build_cache_reclaim_ttl_secs", LIVE),
    page("sandbox_build_cache_reclaim_interval_secs", LIVE),
    page("sandbox_build_cache_reclaim_high_watermark_pct", LIVE),
    page("sandbox_build_cache_reclaim_low_watermark_pct", LIVE),
    page("sandbox_build_cache_reclaim_max_candidates", LIVE),
    page("sandbox_max_source_roots", LIVE),
    page("sandbox_min_free_gib", LIVE),
    // Issue #955: the purge worker reads the toggle at every 10-minute tick.
    page("archived_sandbox_purge_enabled", LIVE),
    // Issue #1007: the queue runner reads the toggle at every tick; batch size
    // and speculation depth are read when a batch is formed.
    page("rolling_queue_enabled", LIVE),
    page("rolling_queue_batch_size", LIVE),
    page("rolling_queue_speculation_depth", LIVE),
    // Issue #1073: the deploy loop reads the toggle at every 5 s poll.
    page("deploy_drain_enabled", LIVE),
    // Issue #794 S3: the scheduler reads these settings on every tick.
    page("program_hold_while_children_run", LIVE),
    page("child_keepalive_enabled", LIVE),
    page("child_keepalive_window_secs", LIVE),
    // #1014: rsid governor.rs reads the policy per admission.
    page("governor_build_slots", LIVE),
    page("governor_lander_slots", LIVE),
    page("governor_max_load", LIVE),
    page("governor_min_free_disk_gb", LIVE),
    page("governor_min_avail_mem_gb", LIVE),
    page("governor_max_workers_slice_gb", LIVE),
    // Issue #792: read when a Harness session launches (its policy is fixed
    // at launch), so a change reaches the next Harness launch.
    page("harness_web_access", NextSpawn),
    page("harness_egress_mode", NextSpawn),
    page("harness_max_search_calls", NextSpawn),
    page("harness_max_fetch_calls", NextSpawn),
    page("harness_max_result_bytes", NextSpawn),
    page("harness_max_web_cost_usd_micros", NextSpawn),
    page("completion_gates_enabled", NextSpawn),
    // #788 slice 4: fixed for the session when its MCP bridge is built.
    page("mcp.deferred_tool_threshold", NextSpawn),
    // #1036: scripts/cloud-spend.py reads the mirrored caps file at every
    // remote run start, so a change reaches the next run.
    page("cloud_spend_stop_line_usd", LIVE),
    page("cloud_spend_daily_cap_usd", LIVE),
    page("agent_build_jobs", NextSpawn),
    page("agent_build_line_tables_only", NextSpawn),
    page("agent_build_sccache_enabled", NextSpawn),
    page("agent_build_sccache_cache_gib", NextSpawn),
    page("agent_build_slots", NextSpawn),
    // Issue #647 systemd scope limits are read by the install/restart path and
    // become effective when the rsid scope is next launched.
    page("rsid_scope_memory_high_mib", DaemonRestart),
    page("rsid_scope_memory_max_mib", DaemonRestart),
    page("rsid_scope_memory_swap_max_mib", DaemonRestart),
    page("rsid_scope_cpu_weight", DaemonRestart),
    // Aggregate worker slice limits are provisioned by the supported launcher.
    page("worker_scope_memory_high_mib", DaemonRestart),
    page("worker_scope_memory_max_mib", DaemonRestart),
    page("worker_scope_memory_swap_max_mib", DaemonRestart),
    page("worker_scope_cpu_weight", DaemonRestart),
    page("vault.env_compat", LIVE),
    page("vault.check_ttl_secs", LIVE),
    // #694: dispatch reads these when a provider session starts.
    page("api_route.openrouter", NextSpawn),
    page("api_route.bedrock", NextSpawn),
    page("api_route.fallback", NextSpawn),
    // #966: the Codex CLI launch and the Harness launch read it per turn.
    page("openrouter_context_budget_tokens", NextSpawn),
    // #1050: the Harness turn reads the iteration cap when it starts.
    page("harness_max_iterations_per_turn", NextSpawn),
];

/// The catalog entry for `field`, if any.
#[must_use]
pub fn daemon_field_spec(field: &str) -> Option<&'static DaemonFieldSpec> {
    DAEMON_CONFIG_FIELDS.iter().find(|spec| spec.field == field)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn catalog_fields_are_unique_and_labelled() {
        let mut seen = BTreeSet::new();
        for spec in DAEMON_CONFIG_FIELDS {
            assert!(seen.insert(spec.field), "duplicate {}", spec.field);
            assert!(!spec.apply.label().is_empty());
            match spec.operator_surface {
                OperatorSurface::SettingsPage => {}
                OperatorSurface::Elsewhere { surface, writer } => {
                    assert!(!surface.is_empty() && !writer.is_empty(), "{}", spec.field);
                }
                OperatorSurface::NoTuiEditor { set_via, tracking } => {
                    assert!(
                        !set_via.is_empty() && !tracking.is_empty(),
                        "{}",
                        spec.field
                    );
                }
            }
        }
        assert_eq!(
            daemon_field_spec("dream_idle_secs").map(|spec| spec.apply),
            Some(ApplyClass::Live)
        );
    }

    #[test]
    fn memory_enabled_applies_off_live_and_on_after_restart() {
        // Turning Memory off gates the worker and drops queued observations
        // while the daemon runs; turning it on needs a restart when no
        // memory worker was started at boot.
        assert_eq!(
            daemon_field_spec("memory_enabled").map(|spec| spec.apply),
            Some(ApplyClass::LiveOffRestartOn)
        );
    }
}
