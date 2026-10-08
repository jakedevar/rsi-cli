# Settings live-apply audit — Issue #1200

Drafted by Codex worker 3b46ff8f on `46eb8ce00`; recovered, reviewed, compiled and
tested by worker 9891ed4c on `origin/rolling` `5706e01ab` (merged into its sandbox).

Eleven daemon settings now have live consumer paths: stall retry, stall detection, reconciliation,
classifier enable/model/four timing-limit fields, and dream observation threshold/cooldown.
System-prompt preset already affected new TUI launches; its restart label was corrected.
No TUI-owned setting needs a TUI restart. “Live” means the next relevant event/tick/request;
it does not rewrite running provider processes, active dream-run caps, or pending handoffs.

Remaining restart boundaries: memory initialization, optional queue/dialectic services,
legacy rotation restoration, and eight systemd resource limits. Reconciliation now stays available while disabled and maintains its watchdog heartbeat.
Signal/iMessage need their own bridge process restarted, now explicitly shown in the UI.

Evidence: `settings_registry.rs` enumerates rows; `settings_keys.rs` mutates App state and
queues daemon actions; `action_handler/daemon_config.rs` applies successful RPC replies;
`rsid/rpc/settings.rs` publishes RuntimeConfig and wakes Dream; `daemon_config_catalog.rs`
records existing consumer locations/classes. Changed consumers are `rsid/main.rs`,
`stall_detector.rs`, `stall_classifier/scheduler.rs`, `dreamer/scheduler.rs`.
Existing classifications below were cross-checked against the catalog and settings apply
paths; service startup boundaries were inspected directly. External Claude loading is
provider-owned and is not represented as a guaranteed in-process reload.

Every registry row is listed, including dynamic lists/actions/read-only rows. Columns
report actual behavior; where existing UI metadata was inaccurate, both are stated.

| Setting (`SettingId`) | Before | After | Reason / consumer |
|---|---|---|---|
| Built-in theme (`BuiltInTheme`) | Live | Live | Theme setter updates active palette; rendering reads it each frame (`settings_keys.rs`). |
| Theme roles (`ThemeRoles`) | Live | Live | Role override setter updates active palette; no widget restart. |
| Legacy message/editor colors (`LegacyColors`) | Live | Live | Color editor updates active palette globals; next render. |
| Reset active theme (`ResetTheme`) | Live | Live | Resets active palette/overrides immediately. |
| Text area background (`TextAreaBackground`) | Live | Live | Renderer reads current App settings on the next frame; no captured startup widget. |
| Background color (`BackgroundColor`) | Live | Live | Renderer reads current App settings on the next frame; no captured startup widget. |
| Formulation animation (`FormulationAnimation`) | Live | Live | Renderer reads current App settings on the next frame; no captured startup widget. |
| Formulation speed (`FormulationSpeed`) | Live | Live | Renderer reads current App settings on the next frame; no captured startup widget. |
| Activity indicator (`ActivityIndicator`) | Live | Live | Renderer reads current App settings on the next frame; no captured startup widget. |
| Detail column (`DetailColumn`) | Live | Live | Renderer reads current App settings on the next frame; no captured startup widget. |
| Navigator preset (`NavigatorPreset`) | Live | Live | Settings mutate App preferences and invalidate card cache; list render reads current settings. Daemon retention fields are read per sweep. |
| Navigator optional columns (`NavigatorColumns`) | Live | Live | Settings mutate App preferences and invalidate card cache; list render reads current settings. Daemon retention fields are read per sweep. |
| Card fields (`CardFields`) | Live | Live | Settings mutate App preferences and invalidate card cache; list render reads current settings. Daemon retention fields are read per sweep. |
| Automatic session archive (`SessionRetentionEnabled`) | Live | Live | Settings mutate App preferences and invalidate card cache; list render reads current settings. Daemon retention fields are read per sweep. Owner: `session_retention_enabled`. |
| Archive idle hours (`SessionRetentionWindowHours`) | Live | Live | Settings mutate App preferences and invalidate card cache; list render reads current settings. Daemon retention fields are read per sweep. Owner: `session_retention_window_hours`. |
| Show system events (`ShowSystemEvents`) | New transcript pane; no restart | New transcript pane; no restart | Default read when opening a transcript; existing pane filter is intentionally independent. |
| Show thinking events (`ShowThinkingEvents`) | New transcript pane; no restart | New transcript pane; no restart | Default read when opening a transcript; existing pane filter is intentionally independent. |
| Hide tool results (`HideToolResults`) | New transcript pane; no restart | New transcript pane; no restart | Default read when opening a transcript; existing pane filter is intentionally independent. |
| Submit on Enter (`SubmitOnEnter`) | Live | Live | Input handling reads current App preferences on the next event. |
| Auto-open question panel (`AutoOpenQuestionPanel`) | Live | Live | Input handling reads current App preferences on the next event. |
| Prompt compiler (`PromptCompiler`) | Live | Live | Toggle rebuilds the prompt processor from current settings in settings_keys.rs. |
| Editing mode (`EditingMode`) | Live | Live | Operator RPC `UpdateDaemonConfig` stores the daemon field; the TUI reads the cached value on each event. Owner: `editing_mode`. |
| Default model (`DefaultModel`) | Live | Live | Updates selected model/provider on App immediately; used by subsequent launches. |
| Title model (`TitleModel`) | Live | Live | Runtime model/provider/URL is read at the next title, extraction, compilation, or dream operation; in-flight work keeps its snapshot. Owner: `title_model_local`, `title_model_provider`, `title_model_base_url`, `title_model_fallback`. |
| Prompt compiler model (`PromptCompilerModel`) | Live | Live | Runtime model/provider/URL is read at the next title, extraction, compilation, or dream operation; in-flight work keeps its snapshot. Owner: `prompt_compile_model_local`, `prompt_compile_model_provider`, `prompt_compile_model_base_url`. |
| Memory model (`MemoryModel`) | Live | Live | Runtime model/provider/URL is read at the next title, extraction, compilation, or dream operation; in-flight work keeps its snapshot. Owner: `memory_model_local`, `memory_model_fallback`, `memory_model_fallback_provider`, `memory_model_fallback_base_url`. |
| Dream model (`DreamModel`) | Live | Live | Runtime model/provider/URL is read at the next title, extraction, compilation, or dream operation; in-flight work keeps its snapshot. Owner: `dream_model`, `dream_model_provider`, `dream_model_base_url`. |
| Stall classifier model (`ClassifierModel`) | Daemon restart | Live | Classifier refreshes model before admission and execution; an in-flight request keeps its original model. Owner: `stall_classifier_model`. |
| API providers (`ApiProviders`) | Live | Live | Edits available endpoint definitions in App; subsequent selections/launches consume them; active sessions retain their route. |
| System prompt preset (`SystemPromptPreset`) | Next TUI launch (mislabelled daemon restart) | Next launch (no daemon/TUI restart) | Already live for subsequent TUI launches: successful save updates App cache; app/session_actions.rs builds the launch prompt from it. Corrected misleading restart metadata. Owner: `system_prompt_preset`. |
| Model control mode (`ModelControlMode`) | Live | Live | Operator RPC updates shared admission/control state; subsequent model calls observe it. Owner: `model control`. |
| Emergency stop (`EmergencyStop`) | Live | Live | Operator RPC updates shared admission/control state; subsequent model calls observe it. Owner: `model control`. |
| Orchestration max child effort (`MaxChildEffort`) | Live | Live | Operator RPC updates shared admission/control state; subsequent model calls observe it. Owner: `orchestration_max_child_effort`. |
| Budget policies (`BudgetPolicies`) | Live | Live | Operator RPC persists policy; model admission reads current policy. Owner: `model budgets`. |
| Usage and telemetry (`UsageStats`) | Live | Live | Read-only telemetry or explicit RPC action; no restart-dependent configuration. Owner: `usage statistics`. |
| Stop-all and cancel (`UsageActions`) | Live | Live | Read-only telemetry or explicit RPC action; no restart-dependent configuration. Owner: `model control`. |
| Provider credential slots (`ProviderCredentialSlots`) | Live | Live | Vault RPC changes credential slots; later credential resolution reads current vault state. Existing provider processes retain launch credentials. Owner: `key vault`. |
| Retry on failure (`RetryOnFailure`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `retry_enabled`. |
| Max retries (`MaxRetries`) | Stored only; no effect | Stored only; no effect | Read-only: default launch policy explicitly returns zero; changing persisted default cannot enable retries. Owner: `retry_max_default`. |
| Retry max backoff (`RetryMaxBackoff`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `retry_max_backoff_ms`. |
| Retry on stall (`RetryOnStall`) | Daemon restart | Live | Subscribe even when disabled at boot; read retry_on_stall before each stall-event retry. Per-session retry policy remains enforced. Owner: `retry_on_stall`. |
| Reconciliation loop (`ReconciliationLoop`) | Daemon restart | Live | Loop starts once and reads runtime enable each tick; disabled passes maintain watchdog heartbeat without touching sessions. Owner: `reconciliation_enabled`. |
| Context rotation (`ContextRotation`) | New launches live; resumed sessions need daemon restart | New launches live; resumed sessions need daemon restart | New launches read runtime switch; restore/continuation paths retain SessionManager boot flag and active rotation state. Updating an in-progress handoff is outside a small safe reload. Owner: `context_rotation_enabled`. |
| Context rotation threshold (global) (`ContextRotationGlobalPct`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `context_rotation_global_pct`. |
| Context rotation threshold (Claude Code) (`ContextRotationClaudePct`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `context_rotation_claude_pct`. |
| Context rotation threshold (Codex) (`ContextRotationCodexPct`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `context_rotation_codex_pct`. |
| Coordinator context cap (0 off) (`CoordinatorContextCap`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `coordinator_context_cap_tokens`. |
| Worker context cap (0 off, 1-100 = % of window) (`WorkerContextCap`) | Live | Live | Read at each usage update of a non-seat session (#1254). Owner: `worker_context_cap_tokens`. |
| Stall detection (`StallDetection`) | Daemon restart | Live | Always start idle detector; each tick gates on runtime enable. Disabled passes keep the duplicate-report set, so re-enabling never re-publishes (or re-remediates) a stall already reported; sessions that stalled while disabled are reported on the first enabled tick. Owner: `stall_detection_enabled`. |
| Stall classifier (`StallClassifier`) | Live OFF; ON needs daemon restart if worker absent | Live | Always wire/start receiver; runtime flag gates both signal emission and receipt. Model admission remains unchanged. Owner: `stall_classifier_enabled`. |
| Classifier idle threshold (Claude) (`ClassifierIdleClaude`) | Daemon restart | Live | Detector reads live idle threshold each tick before deciding whether to signal. Owner: `stall_classifier_idle_secs`. |
| Classifier idle threshold (Codex) (`ClassifierIdleCodex`) | Daemon restart | Live | Detector reads live Codex-family idle threshold each tick. Owner: `stall_classifier_idle_secs_codex`. |
| Classifier cooldown (`ClassifierCooldown`) | Daemon restart | Live | Detector reads live per-session cooldown each tick; retained timestamps preserve elapsed time. Owner: `stall_classifier_cooldown_secs`. |
| Classifier max per session (`ClassifierMaxPerSession`) | Daemon restart | Live | Detector reads live lifetime cap each tick; classification count is retained. Owner: `stall_classifier_max_per_session`. |
| Classifier confidence floor (`ClassifierConfidenceFloor`) | Live | Live | Runtime confidence floor is read for each classification. Owner: `stall_classifier_confidence_floor`. |
| Memory system (`MemorySystem`) | Live OFF; ON needs daemon restart if worker absent | Live OFF; ON needs daemon restart if worker absent | Boot initialization opens a separate memory store, creates embedding provider and injects optional manager into multiple services. No safe small initializer/rebinding path; existing worker can resume live. Owner: `memory_enabled`. |
| Dream consolidation (`DreamConsolidation`) | Live | Live | Dream scheduler reads runtime control each cycle. Owner: `dream_enabled`. |
| Observation threshold (`ObservationThreshold`) | Daemon restart | Live | Dream scheduler refreshes threshold on control wake/tick; existing UpdateDaemonConfig already notifies it. Owner: `dream_observation_threshold`. |
| Dream cooldown (`DreamCooldown`) | Daemon restart | Live | Scheduler snapshots live cooldown into future runs; active-run persisted caps and already scheduled cooldown deadlines remain fixed. Owner: `dream_cooldown_secs`. |
| Dream idle wait (`DreamIdle`) | Live | Live | Dream scheduler reads runtime control each cycle. Owner: `dream_idle_secs`. |
| Dialectic engine (`DialecticEngine`) | Daemon restart | Daemon restart | RPC server owns optional engine constructed at startup with memory manager/model endpoint dependencies. No existing runtime reconstruction path; left restart-only. Owner: `dialectic_enabled`. |
| Background queue (`BackgroundQueue`) | Daemon restart | Daemon restart | Boot constructs optional queue handle injected into SessionManager. Enabling requires installing that handle; unconditional construction would also change enqueue-while-disabled semantics. Left restart-only. Owner: `queue_enabled`. |
| Recursive DAG recovery controls (`DagRecoveryControls`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `recursive_dag_recovery_controls_enabled`. |
| Recursive DAG scheduler controls (`DagSchedulerControls`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `recursive_dag_scheduler_controls_enabled`. |
| Recursive DAG cancellation controls (`DagCancellationControls`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `recursive_dag_cancellation_controls_enabled`. |
| Recursive DAG live scheduler (`DagLiveSchedulerControl`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `recursive_dag_live_scheduler_control_enabled`. |
| Recursive DAG run lease TTL (`DagRunLeaseTtl`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `recursive_dag_run_lease_ttl_ms`. |
| Recursive DAG max concurrent graphs (`DagMaxConcurrentGraphs`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `recursive_dag_max_concurrent_graphs`. |
| Graph overlay: render recursive origin (`GvRenderRecursiveOrigin`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `gv_render_recursive_origin`. |
| Graph overlay: info dashboard (`GvInfoDashboard`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `gv_info_dashboard`. |
| Follow agent-created projects (`FollowAgentCreatedProjects`) | Live | Live | The daemon stores the flag; the TUI reads its cached copy when a `project_created` event arrives. Owner: `follow_agent_created_projects`. |
| Durable topology executor (`TopologyExecutor`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `topology_executor_enabled`. |
| Topology build nodes (`TopologyBuildNodes`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `topology_max_concurrent_build_nodes`. |
| Topology bulk fan-out on OpenRouter (0 off) (`TopologyBulkFanout`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `topology_bulk_fanout_min_openrouter`. |
| Completed transcript cache (bytes) (`CompletedTranscriptCache`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `completed_transcript_cache_max_bytes`. |
| rsid MemoryHigh (MiB) (`RsidScopeMemoryHigh`) | Daemon restart | Daemon restart | Installer/launcher provisions systemd scope or aggregate worker slice from saved snapshot. Existing processes keep their resource scope; daemon restart required. Owner: `rsid_scope_memory_high_mib`. |
| rsid MemoryMax (MiB) (`RsidScopeMemoryMax`) | Daemon restart | Daemon restart | Installer/launcher provisions systemd scope or aggregate worker slice from saved snapshot. Existing processes keep their resource scope; daemon restart required. Owner: `rsid_scope_memory_max_mib`. |
| rsid MemorySwapMax (MiB) (`RsidScopeMemorySwapMax`) | Daemon restart | Daemon restart | Installer/launcher provisions systemd scope or aggregate worker slice from saved snapshot. Existing processes keep their resource scope; daemon restart required. Owner: `rsid_scope_memory_swap_max_mib`. |
| rsid CPUWeight (`RsidScopeCpuWeight`) | Daemon restart | Daemon restart | Installer/launcher provisions systemd scope or aggregate worker slice from saved snapshot. Existing processes keep their resource scope; daemon restart required. Owner: `rsid_scope_cpu_weight`. |
| Worker slice MemoryHigh (MiB) (`WorkerScopeMemoryHigh`) | Daemon restart | Daemon restart | Installer/launcher provisions systemd scope or aggregate worker slice from saved snapshot. Existing processes keep their resource scope; daemon restart required. Owner: `worker_scope_memory_high_mib`. |
| Worker slice MemoryMax (MiB) (`WorkerScopeMemoryMax`) | Daemon restart | Daemon restart | Installer/launcher provisions systemd scope or aggregate worker slice from saved snapshot. Existing processes keep their resource scope; daemon restart required. Owner: `worker_scope_memory_max_mib`. |
| Worker slice MemorySwapMax (MiB) (`WorkerScopeMemorySwapMax`) | Daemon restart | Daemon restart | Installer/launcher provisions systemd scope or aggregate worker slice from saved snapshot. Existing processes keep their resource scope; daemon restart required. Owner: `worker_scope_memory_swap_max_mib`. |
| Worker slice CPUWeight (`WorkerScopeCpuWeight`) | Daemon restart | Daemon restart | Installer/launcher provisions systemd scope or aggregate worker slice from saved snapshot. Existing processes keep their resource scope; daemon restart required. Owner: `worker_scope_cpu_weight`. |
| Rolling merge queue (`RollingQueueEnabled`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `rolling_queue_enabled`. |
| Detach Claude turns (`TurnDetachEnabled`) | Live | Live | `ClaudeClient::launch` reads the shared `turn_detach_enabled` boolean for each new managed Claude turn; an existing turn keeps its pipe or spool reader. Owner: `turn_detach_enabled`. |
| Hold new work while a deploy waits (`DeployDrainEnabled`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `deploy_drain_enabled`. |
| Deploy hold limit (s) (`DeployDrainHold`) | Live | Live | The deploy runner reads the cap at every 5 s poll; a shorter cap releases a waiting deploy's hold on its next poll. Owner: `deploy_drain_hold_secs`. |
| Host load limit for new launches (`HostLoadAdmissionThreshold`) | Live | Live | The manager-action claim loop (every pass, at most 10 s apart) and each topology node launch read the threshold at the moment they decide; raising or zeroing it releases held launches on the next pass, lowering it holds queued ones. Owner: `host_load_admission_threshold`. |
| Merge queue batch size (`RollingQueueBatchSize`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `rolling_queue_batch_size`. |
| Merge queue speculation depth (`RollingQueueSpeculationDepth`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `rolling_queue_speculation_depth`. |
| Merge queue gate timeout (min) (`RollingQueueGateTimeout`) | Live | Live | Read when the queue claims its next batch (the batch gate deadline); a batch already gating keeps the deadline it was claimed with. Owner: `rolling_queue_gate_timeout_mins`. |
| Agent test job timeout (min) (`JobTestTimeout`) | Live | Live | Read by `AgentSubmitJob` at each submit and stamped into the test job's params (#1337); a running job keeps the timeout it was submitted with. Owner: `job_test_timeout_mins`. |
| CPU andon: CPU-minutes per tree (`CpuAndonCpuMinutes`) | Live | Live | Read by `rsid::cpu_andon` at every one-minute sample (#1337); 0 turns the trigger off. Owner: `cpu_andon_cpu_minutes`. |
| CPU andon: host load (`CpuAndonHostLoad`) | Live | Live | Read by `rsid::cpu_andon` at every one-minute sample (#1337); 0 turns the trigger off. Owner: `cpu_andon_host_load`. |
| Hold program wakes while children run (`ProgramHoldWhileChildren`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `program_hold_while_children_run`. |
| Child keep-alive valve (`ChildKeepaliveEnabled`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `child_keepalive_enabled`. |
| Child keep-alive window (s) (`ChildKeepaliveWindow`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `child_keepalive_window_secs`. |
| Build slots (`GovernorBuildSlots`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_build_slots`. |
| Lander slots (`GovernorLanderSlots`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_lander_slots`. |
| Governor max load (0 auto) (`GovernorMaxLoad`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_max_load`. |
| Governor min free disk (GB) (`GovernorMinFreeDisk`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_min_free_disk_gb`. |
| Governor min available memory (GB) (`GovernorMinAvailMem`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_min_avail_mem_gb`. |
| Governor max workers-slice memory (GB) (`GovernorMaxWorkersSlice`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_max_workers_slice_gb`. |
| Harness web access (`HarnessWebAccess`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_web_access`. |
| Harness network egress (`HarnessEgressMode`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_egress_mode`. |
| Harness context editing (`HarnessContextEditing`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_context_editing`. |
| Harness search call cap (`HarnessSearchCap`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_search_calls`. |
| Harness fetch call cap (`HarnessFetchCap`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_fetch_calls`. |
| Harness completion gates (`HarnessCompletionGates`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `completion_gates_enabled`. |
| Harness tool output cap (bytes) (`HarnessOutputCap`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_result_bytes`. |
| Harness web cost cap (micro-USD) (`HarnessWebCostCap`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_web_cost_usd_micros`. |
| MCP deferred tool threshold (`McpDeferredToolThreshold`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `mcp.deferred_tool_threshold`. |
| Cloud spend (`CloudSpendStatus`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `cloud spend`. |
| Cloud spend stop line (USD) (`CloudSpendStopLine`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `cloud_spend_stop_line_usd`. |
| Cloud spend daily cap (USD) (`CloudSpendDailyCap`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `cloud_spend_daily_cap_usd`. |
| Codegraph indexing (`CodegraphIndexing`) | Live | Live | Indexer uses shared runtime flag and publication gate; acknowledged OFF prevents subsequent publication. Owner: `codegraph_indexing_enabled`. |
| Codex sandbox (`CodexSandbox`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `codex_sandbox_mode`. |
| Claude project config isolation (`ClaudeConfigIsolation`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `claude_config_isolation`. |
| Vault: legacy env fallback (`VaultEnvCompat`) | Live | Live | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Owner: `vault.env_compat`. |
| Vault: check TTL (`VaultCheckTtl`) | Live | Live | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Owner: `vault.check_ttl_secs`. |
| OpenRouter engine (`OpenRouterRoute`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `api_route.openrouter`. |
| Bedrock engine (`BedrockRoute`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `api_route.bedrock`. |
| Provider profile (`ProviderProfile`) | Live | Live | #1407: `RuntimeConfig::launch_model_refusal` reads the profile at every launch, continuation, rotation and spawn check; the TUI pickers re-filter when the row changes. Running sessions are not stopped. Owner: `provider_profile`. |
| API route fallback (`ApiRouteFallback`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `api_route.fallback`. |
| OpenRouter context budget (0 off) (`OpenRouterContextBudget`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `openrouter_context_budget_tokens`. |
| Harness iterations per turn (`HarnessMaxIterations`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_iterations_per_turn`. |
| Sandbox storage (`SandboxStorageStatus`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox storage`. |
| Sandbox cache reclaim (`CacheReclaim`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_build_cache_reclaim_enabled`. |
| Cache reclaim TTL (`CacheReclaimTtl`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_build_cache_reclaim_ttl_secs`. |
| Cache reclaim interval (`CacheReclaimInterval`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_build_cache_reclaim_interval_secs`. |
| Cache pressure high (`CachePressureHigh`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_build_cache_reclaim_high_watermark_pct`. |
| Cache pressure low (`CachePressureLow`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_build_cache_reclaim_low_watermark_pct`. |
| Cache reclaim pass limit (`CacheReclaimPassLimit`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_build_cache_reclaim_max_candidates`. |
| Agent build jobs (`AgentBuildJobs`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Next-launch policy is deliberately stable for an already running session/build. Owner: `agent_build_jobs`. |
| Agent line tables (`AgentBuildLineTables`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Next-launch policy is deliberately stable for an already running session/build. Owner: `agent_build_line_tables_only`. |
| Worker sccache (`AgentBuildSccache`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Next-launch policy is deliberately stable for an already running session/build. Owner: `agent_build_sccache_enabled`. |
| sccache cap (GiB) (`AgentBuildSccacheSize`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Next-launch policy is deliberately stable for an already running session/build. Owner: `agent_build_sccache_cache_gib`. |
| Machine build slots (`AgentBuildSlots`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Next-launch policy is deliberately stable for an already running session/build. Owner: `agent_build_slots`. |
| Preview cache reclaim (`PreviewReclaim`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox storage`. |
| Reclaim sandbox caches now (`ReclaimNow`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox storage`. |
| Source-worktree settlement (`SourceWorktreeSettlement`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `source-worktree settlement`. |
| Legacy scratch adoption (`LegacyScratchAdoption`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `legacy scratch adoption`. |
| Maximum sandbox roots (`SandboxMaxSourceRoots`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_max_source_roots`. |
| Minimum free space (GiB) (`SandboxMinFreeGib`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_min_free_gib`. |
| Purge archived sandboxes (`ArchivedSandboxPurge`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `archived_sandbox_purge_enabled`. |
| Claude hooks (`ClaudeHooks`) | Live | Live | File saved immediately; Claude owns consumption at its next supported load boundary. No TUI/daemon restart; cannot promise mutation of active provider hook state. Owner: `~/.claude/settings.json`. |
| Claude skills (`ClaudeSkills`) | Live | Live | File enable/disable applies immediately to inventory; Claude owns its next skill load. No TUI/daemon restart. Owner: `~/.claude/skills/`. |
| Signal bridge (`SignalBridge`) | Bridge restart (mislabelled live) | Bridge process restart | TUI writes signal.toml; flywheel-signal/src/main.rs loads once. Restart the independently launched bridge, not rsid; no reload control exists. Owner: `signal.toml`. |
| iMessage bridge (`ImessageBridge`) | Bridge restart (mislabelled live) | Bridge process restart | TUI writes imessage.toml; flywheel-imessage/src/main.rs loads once. Restart the independently launched bridge, not rsid; no reload control exists. Owner: `imessage.toml`. |
| Satellite registry (`SatelliteRegistry`) | Live | Live | Registry RPCs update current state; poller reads runtime enable on its next tick. Owner: `satellite_registry`. |
| Satellite polling (`SatellitePolling`) | Live | Live | Registry RPCs update current state; poller reads runtime enable on its next tick. Owner: `satellite_polling_enabled`. |
| Remote access (`RemoteAccess`) | Live | Live | Operator RPC changes managed gateway and authorization state; disabling denies subsequent requests. Owner: `remote_access`. |
| MCP servers (`McpServerConfigurations`) | Live inventory; next session MCP bridge | Live inventory; next session MCP bridge | RPC persists definition/credential edits immediately; session MCP bridges snapshot definitions when built (next launch for existing sessions). Owner: `mcp servers`. |

## Verification and custody

Review notes (worker 9891ed4c): every conversion keeps the consumer task alive
from boot and reads the `RuntimeConfig` atomic at the next tick/event, so a save
races only with the work already admitted (an in-flight classification keeps its
model, an active dream run keeps its persisted caps, an already scheduled dream
cooldown keeps its deadline). One draft behaviour was changed: the stall detector
no longer clears its reported set while disabled (see the Stall detection row).
`stall_classifier_api_url`/`_api_key`/`_timeout_secs` are not Settings-page rows and
stay boot-only; only the model name is refreshed per classification.

- `cargo test -p rsid --lib -- stall_detector stall_classifier::scheduler dreamer::scheduler reconciliation::tests`:
  67 passed, 0 failed (`/tmp/i1200/rsid-scoped.log`).
- `cargo check -p rsid --bin rsid`: clean (`/tmp/i1200/rsid-bin.log`).
- `cargo test -p rsi-common --lib daemon_config_catalog`: 2 passed (`/tmp/i1200/common.log`).
- `cargo test -p rsi --lib` (full): 2046 passed, 0 failed, including the `manual::`
  generator parity tests (`/tmp/i1200/rsi-full.log`). No baseline reds hit.
- New tests sit in shards `other-04` (stall detector, classifier, dream) and
  `other-05` (reconciliation).
