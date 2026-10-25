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
    /// Read at use time; takes effect as soon as the change is applied (the
    /// Settings page applies on `w`), with no restart.
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
            Self::Live => "when applied · no restart",
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

/// Builds a `NoTuiEditor` entry: a persisted field with no settings row.
/// (Slice (c) of Issue #35 wired its last two gap fields onto rows; #2236 made
/// `retry_max_default` a field that has none.)
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
    // #2236: the settings row is gone (it only showed a stored number nothing
    // reads for launches). The key survives as the boot seed of the retry
    // kill-switch (`RSI_RETRY_MAX_DEFAULT` > 0 turns `retry_enabled` on) and a
    // nonzero write still re-enables `retry_enabled`.
    no_tui_editor(
        "retry_max_default",
        ApplyClass::NotApplied,
        "`RSI_RETRY_MAX_DEFAULT` at daemon start, or `UpdateDaemonConfig`; stored, not applied to default launches (only seeds the retry kill-switch)",
        "#2236",
    ),
    // The subscriber stays alive; runtime retry_on_stall gates each event.
    page("retry_on_stall", LIVE),
    // rsid session/lifecycle.rs:4206 loads the atomic per backoff.
    page("retry_max_backoff_ms", LIVE),
    // Runtime-gated loop remains alive; disabled passes maintain the watchdog heartbeat.
    page("reconciliation_enabled", LIVE),
    // The detector stays alive; the runtime switch gates each tick.
    page("stall_detection_enabled", LIVE),
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
    // Dream scheduler refreshes future-run policy on every control wake/tick.
    page("dream_observation_threshold", LIVE),
    // rsid dreamer/scheduler.rs:593 loads the atomic per cycle.
    page("dream_idle_secs", LIVE),
    // New runs snapshot the live cooldown; existing run caps stay immutable.
    page("dream_cooldown_secs", LIVE),
    // Prompt compiler: rsid prompt_compile/engine.rs:102-111 reads per compile.
    page("prompt_compile_model_local", LIVE),
    page("prompt_compile_model_provider", LIVE),
    page("prompt_compile_model_base_url", LIVE),
    // S-048 (rsid codex.rs:342).
    page("codex_sandbox_mode", NextSpawn),
    // S-049 (rsid claude.rs:484).
    page("claude_config_isolation", NextSpawn),
    // #2760 (rsid codex.rs, read per spawn).
    page("codex_config_isolation", NextSpawn),
    // hub #2765: the Claude and Codex CLI launches read it per spawn.
    page("provider_launch_overrides", NextSpawn),
    // #2762 (rsid claude.rs, read per Claude CLI spawn).
    page("claude_fallback_models", NextSpawn),
    // #3176: the operator allowlist of docker hosts for Harness container exec
    // targets (read at every container-bound launch, continuation and rotation).
    page("bench.docker_hosts", NextSpawn),
    // #3181: the benchmark job (read at every submit and start of a `bench` job).
    page("bench.enabled", NextSpawn),
    page("bench.max_concurrency", NextSpawn),
    page("bench.laptop_host", NextSpawn),
    page("claude_max_budget_usd", NextSpawn),
    // #3154 (rsid openrouter_spend.rs, read at each launch and model call).
    page("openrouter_spend_cap_usd", LIVE),
    page("openrouter_spend_warn_pct", LIVE),
    no_tui_editor(
        "openrouter_spend_baseline_usd",
        LIVE,
        "written by arming openrouter_spend_cap_usd (a fresh probe of the account usage)",
        "#3154",
    ),
    no_tui_editor(
        "openrouter_spend_armed_at",
        LIVE,
        "written by arming openrouter_spend_cap_usd (the time of that probe)",
        "#3154",
    ),
    // The TUI updates its cache after save and includes the preset in launches.
    page("system_prompt_preset", NextSpawn),
    // #1626: the TUI reads it from GetDaemonConfig when a `project_created`
    // event arrives; the daemon only stores it.
    page("follow_agent_created_projects", LIVE),
    // Classifier wiring stays alive; detector and receiver both gate on runtime.
    page("stall_classifier_enabled", LIVE),
    // The classifier snapshots the live model before admission and execution.
    page("stall_classifier_model", LIVE),
    // Detector refreshes all four classifier gates every tick.
    page("stall_classifier_idle_secs", LIVE),
    page("stall_classifier_idle_secs_codex", LIVE),
    page("stall_classifier_cooldown_secs", LIVE),
    page("stall_classifier_max_per_session", LIVE),
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
    // settings row (PROVIDERS & SANDBOXES > API Routes / Harness Tools) that writes them via
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
    // Issue #1628: operator text-editing mode. Operator-view setting read by
    // the TUI on every config refresh; no daemon behavior depends on it.
    page("editing_mode", LIVE),
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
    // Issue #3427: the disposition loop reads its switch every five minutes,
    // and grace/budget at the start of each bounded pass (rsid session/error_disposition.rs).
    page("error_disposition_enabled", LIVE),
    page("error_disposition_grace_minutes", LIVE),
    page("error_disposition_continuation_budget", LIVE),
    // Issue #2384: the cold-archive loop reads the toggle at every 10-minute tick,
    // and the age and directory at the start of each pass (rsid
    // session/cold_archive.rs).
    page("cold_archive_enabled", LIVE),
    page("cold_archive_after_days", LIVE),
    // The settings row opens a text editor overlay (#2438).
    page("cold_archive_dir", LIVE),
    // Issue #1882: read when a seed starts a pool build and on every reclaim pass.
    page("target_pool_keep", LIVE),
    // Issue #955: the purge worker reads the toggle at every 10-minute tick.
    page("archived_sandbox_purge_enabled", LIVE),
    // Settlement reads both at each tick, before intent and before removal.
    page("source_worktree_auto_settlement_enabled", LIVE),
    page("disk_pressure_min_free_gb", LIVE),
    // Issue #1007: the queue runner reads the toggle at every tick; batch size
    // is read when a batch is formed. (#2236: the speculation-depth setting is
    // gone; no queue code read it.)
    page("rolling_queue_enabled", LIVE),
    page("rolling_queue_batch_size", LIVE),
    // Issue #1208: read when a batch is claimed (its gate deadline).
    page("rolling_queue_gate_timeout_mins", LIVE),
    // Issue #2112: the full-library QA task reads it before each dispatch.
    page("rolling_queue_full_qa_interval_mins", LIVE),
    // Issue #2509: the job launcher reads the QA target size cap at each
    // full-library QA start.
    page("qa_target_max_gib", LIVE),
    // Issue #86: the paste retention pass reads it at each pass (daemon start
    // and after an upload, at most hourly).
    page("paste_retention_referenced_days", LIVE),
    // Issue #2752: the Codex live-catalog cache reads its TTL at each lookup.
    page("codex_live_catalog_ttl_secs", LIVE),
    // Issue #2757: the Codex plan-window poll loop reads its interval each cycle.
    page("codex_rate_limit_poll_secs", LIVE),
    page("claude_billing_mode", LIVE),
    page("codex_billing_mode", LIVE),
    page("codexappserver_billing_mode", LIVE),
    page("pioneer_billing_mode", LIVE),
    page("antigravity_billing_mode", LIVE),
    // Issue #2763: each Claude/Codex spawn reads the subagent model and cap.
    page("claude_subagent_model", NextSpawn),
    page("claude_max_concurrent_subagents", NextSpawn),
    page("codex_subagent_model", NextSpawn),
    page("codex_max_concurrent_subagents", NextSpawn),
    // Issue #2766: a Claude or Codex launch reads the lifecycle hook set.
    page("provider_lifecycle_hooks", NextSpawn),
    // Issue #2767: a Claude or Codex launch reads how its skills are delivered.
    page("provider_skill_delivery", NextSpawn),
    // Issue #2755: the canary reads its switch at each run and the launch
    // refusal reads its switch at each Codex launch.
    page("provider_contract_canary", LIVE),
    page("codex_refuse_untrusted_custody_hook", LIVE),
    // Issue #2759: the topology executor reads it when it opens a node's
    // handoff turn.
    page("structured_handoff_default", LIVE),
    // Issue #2524: the job launcher reads the thermal ceiling at each job start.
    page("job_thermal_ceiling_c", LIVE),
    // Issue #2747: the scheduler reads the health thresholds at each
    // `when.health_anomaly` evaluation.
    page("health_disk_used_pct", LIVE),
    page("health_thermal_c", LIVE),
    page("health_stuck_minutes", LIVE),
    page("health_provider_window_used_pct", LIVE),
    // Issue #1337: AgentSubmitJob reads the test-job timeout default at each
    // submit; the CPU-time andon reads its two thresholds at every sample.
    page("job_test_timeout_mins", LIVE),
    page("cpu_andon_cpu_minutes", LIVE),
    page("cpu_andon_host_load", LIVE),
    // Issue #1073: the deploy loop reads the toggle at every 5 s poll.
    page("turn_detach_enabled", LIVE),
    page("deploy_drain_enabled", LIVE),
    // Issue #1320/#1311: the deploy loop reads the hold cap at every 5 s poll.
    page("deploy_drain_hold_secs", LIVE),
    // Issue #1777: every deploy request and quiet-point restart reads the
    // budget (the deploy loop refreshes it every 5 s poll).
    page("deploy_restarts_per_hour", LIVE),
    // Issue #2561: the supervisor re-reads the published grace period when a
    // daemon shutdown begins (the snapshot is rewritten on every accepted change).
    page("supervisor_shutdown_grace_secs", LIVE),
    // Issue #1417: every manager create_session admission reads the threshold.
    page("host_load_admission_threshold", LIVE),
    page("worker_build_reserve_gb", LIVE),
    // Issue #1992: each worker scope reads the quota when it is created.
    page("worker_cpu_quota_percent", NextSpawn),
    // Issue #2119: the daemon rewrites rsi-build.slice's CPUQuota on every
    // accepted change; job launchers read the share for their own ceilings.
    page("hub_build_cpu_percent", LIVE),
    // Issue #2373: the event writer reads the cap for every ToolResult it
    // stores; rows already stored keep the body they have.
    page("tool_result_storage_cap_kib", LIVE),
    // Issue #794 S3: the scheduler reads these settings on every tick.
    page("program_hold_while_children_run", LIVE),
    page("child_keepalive_enabled", LIVE),
    page("child_keepalive_window_secs", LIVE),
    // #1014: rsid governor.rs reads the policy per admission.
    page("governor_build_slots", LIVE),
    page("governor_lander_slots", LIVE),
    page("governor_check_slots", LIVE),
    page("governor_max_load", LIVE),
    page("governor_min_free_disk_gb", LIVE),
    page("governor_min_avail_mem_gb", LIVE),
    page("governor_max_workers_slice_gb", LIVE),
    // Issue #792: read when a Harness session launches (its policy is fixed
    // at launch), so a change reaches the next Harness launch.
    page("harness_web_access", NextSpawn),
    page("harness_egress_mode", NextSpawn),
    // Issue #2251: read when a Harness session launches (it decides whether
    // the native `web_search` tool is advertised).
    page("harness_web_search_backend", NextSpawn),
    // Issue #2315: the SearXNG base URL, read at the same launch.
    page("harness_searxng_url", NextSpawn),
    // #1111: the Harness context-editing default, read at launch.
    page("harness_context_editing", NextSpawn),
    page("harness_max_search_calls", NextSpawn),
    page("harness_max_fetch_calls", NextSpawn),
    page("harness_max_result_bytes", NextSpawn),
    page("harness_max_web_cost_usd_micros", NextSpawn),
    page("completion_gates_enabled", NextSpawn),
    // #788 slice 4: fixed for the session when its MCP bridge is built.
    page("mcp.deferred_tool_threshold", NextSpawn),
    // #1036: rsi-cloud spend reads the mirrored caps file at every
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
    // #3141: one opt-in per provider. The legacy `api_route.fallback` is kept
    // as an alias of the Bedrock value only (it has no row of its own).
    no_tui_editor(
        "api_route.fallback",
        NextSpawn,
        "`UpdateDaemonConfig` (legacy alias of `api_route.fallback.bedrock`; never touches OpenRouter); the Bedrock fallback row is the editor",
        "#3141",
    ),
    page("api_route.fallback.openrouter", NextSpawn),
    page("api_route.fallback.bedrock", NextSpawn),
    // #966: the Codex CLI launch and the Harness launch read it per turn.
    page("openrouter_context_budget_tokens", NextSpawn),
    // #1050: the Harness turn reads the iteration cap when it starts.
    page("harness_max_iterations_per_turn", NextSpawn),
    // #1005: rsid session/context_cap.rs reads the cap at each usage update
    // of a coordinating seat. Overrides are `coordinator_context_cap.<key>`
    // fields written by the TUI command `:context-cap`.
    page("coordinator_context_cap_tokens", LIVE),
    // Issue #4021: the scheduler reads the cap from the persisted row at each
    // manager-notice delivery.
    page("manager_max_turns_per_hour", LIVE),
    // #1254: rsid session/context_cap.rs reads the worker baton cap at each
    // usage update of a non-seat session. Overrides are
    // `worker_context_cap.<key>` fields written by `:worker-context-cap`.
    page("worker_context_cap_tokens", LIVE),
    // #1407: `RuntimeConfig::launch_model_refusal` reads the profile at every
    // launch check; the region is read at each Bedrock launch and check.
    page("provider_profile", LIVE),
    // Written by the TUI command `:aws-setup <region>` (the first-run AWS
    // setup step, which also stores the Bedrock key and verifies it).
    DaemonFieldSpec {
        field: crate::provider_profile::BEDROCK_REGION_FIELD,
        apply: NextSpawn,
        operator_surface: OperatorSurface::Elsewhere {
            surface: ":aws-setup command",
            writer: "action_handler/aws_setup.rs",
        },
    },
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
