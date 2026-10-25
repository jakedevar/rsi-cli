# Settings live-apply audit — Issue #1200

## New-setting checklist

1. Add the registry row in `crates/rsi/src/settings_registry.rs`, with its owner,
   apply class and consumer wired. Ship the operator RPC and TUI surface together
   (AGENTS.md rule 10); keep operator-only methods out of agent catalogs.
2. Add a row below containing the literal `` (`SettingIdVariant`) `` and its
   owner key, consumer and live-apply outcome. The Rust audit test remains the
   authority for coverage and converted controls' live-apply class.
3. Run `node desktop/scripts/settings.mjs` from the repository root and commit
   `desktop/src/settings.generated.ts`. Map the row in `widgetFor` /
   `OVERLAY_ROW_IDS` in `desktop/src/settings-model.ts`; intentional TUI-only
   rows also need `TUI_ONLY` in `desktop/tests/settings-state.test.mjs` and
   `tuiOnlyRows` in `desktop/parity-audit.json`.
4. Run `make manual` (the daemon's `manual` test recipe), and commit regenerated
   `docs/manual/rsi-manual.md`, `docs/manual/rsi-manual.html` and any changed
   generated regions of `docs/keybindings.md`. Then run
   `node desktop/scripts/keymap.mjs` and `node desktop/scripts/overlay-help.mjs`
   and commit changed generated catalogs. A new overlay also needs owned parity
   entries and the corresponding parity fixture count. Update explanatory docs as needed.
5. Run `scripts/repo-guard settings-preflight origin/rolling HEAD` after
   committing. `scripts/scoped-test` runs this omission check before Cargo for
   settings diffs, reusing `node desktop/scripts/settings.mjs --check`. Full Rust
   audit and manual drift tests remain authoritative.

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


## Issue #2236 — staged edits, one daemon update, consumer audit and tabs

**Staging.** Since #2241 the Settings page stages an edit locally (`+`/`-`/Enter) and applies
every pending row with `w` (Esc or `q` discards). "Live" in the tables below therefore means
"takes effect on the next event, tick or request *after the page is applied*". The info card's
chip says so: `on apply (w)` for a staged row, `applies now` for a row that acts when used (a
theme preview, a dropdown pick, a form), `runs when activated` for an action button.

**One daemon update per apply.** `w` sends ONE `UpdateDaemonConfig` carrying
`{"updates": [{"field", "value"}, ...]}`. The daemon validates every value before any live
state moves (an invalid value, an unknown or repeated field, an invalid combination such as a
low cache watermark above the high one, or an `rsid` scope limit set that is not ordered, refuses
the whole request and names the field), writes the durable rows in one SQLite transaction,
publishes the live values once, and runs each save side effect (supervisor and scope snapshot
files, spend-caps file, build CPU slice, dreamer wake) at most once. A failure after the live
values moved restores them and the rows. The model-control mode is a policy, not a config
field, so it keeps its own RPC. The request is operator-only like `UpdateDaemonConfig`; it is
not in `AGENT_VERBS`, `READ_VERBS` or any native tool.

**Tabs.** The oversized sections were split by the question they answer (page order; the group
is the rail entry): AGENT AUTOMATION: Retries & Recovery, Context Rotation, Stall Detection,
Memory & Dreaming, Scheduling, Recursive DAG, Code Intelligence. SAFETY & SPEND gained Cloud
Spend. PROVIDERS & SANDBOXES: Provider Isolation, Provider Skills, API Routes, Harness Tools, Claude Hooks,
Claude Skills. HOST & BUILD (new group): Resource Limits, Build Governor, Queue & Deploys,
Sandbox Storage, Cold Storage (#2384), Build Cache, Agent Builds. No tab holds more than twelve rows
(`settings_registry::tests::no_section_holds_more_than_a_dozen_rows`). Search (`/`, `n`, `N`)
walks `SETTINGS`, so every row stays findable, and the tab name is itself a search term.

**Audit.** Every row's owner key was traced to a consumer that changes behavior with the value
(the table below; "kept" means a consumer exists). Two rows were vestigial and are removed:
`Max retries` (`retry_max_default`: stored and shown, discarded by the retry policy; the key
remains as the boot seed of the retry kill-switch, listed in the manual's no-editor table) and
`Merge queue speculation depth` (`rolling_queue_speculation_depth`: only echoed by
`GetRollingQueue`; the key, its status field and its catalog entry are gone; a stale
`daemon_settings` row is ignored at load). No row was found that should work but is not wired.

| Row (`SettingId`) | Tab | Key / owner | Consumer | Outcome |
|---|---|---|---|---|
| Built-in theme (`BuiltInTheme`) | Theme & Colors | TUI state | `ui/theme.rs` active palette, read every frame | kept |
| Theme roles (`ThemeRoles`) | Theme & Colors | TUI state | `ui/theme.rs::set_theme_role_override`, read every frame | kept |
| Reset active theme (`ResetTheme`) | Theme & Colors | TUI state | `ui/theme.rs` role overrides cleared | kept |
| Text area background (`TextAreaBackground`) | Screen | TUI state | `settings.rs::resolve_text_area_backfill` (input surfaces) | kept |
| Background color (`BackgroundColor`) | Screen | TUI state | `settings.rs::text_area_bg_color`; `overlay/text_area_bg_editor.rs` | kept |
| Formulation animation (`FormulationAnimation`) | Screen | TUI state | `ui/session.rs:5333`, `ui/mod.rs:747` | kept |
| Formulation speed (`FormulationSpeed`) | Screen | TUI state | `ui/session.rs:5334`, `ui/mod.rs:748` | kept |
| Activity indicator (`ActivityIndicator`) | Screen | TUI state | `ui/session.rs:4874`, `ui/mod.rs:703` | kept |
| Detail column (`DetailColumn`) | Screen | TUI state | `ui/mod.rs:671` (also Ctrl-Left/Right nudges) | kept |
| Navigator preset (`NavigatorPreset`) | Session List | TUI state | `ui/session.rs:1406` | kept |
| Navigator optional columns (`NavigatorColumns`) | Session List | TUI state | `ui/session.rs:1407` | kept |
| Card fields (`CardFields`) | Session List | TUI state | `types/row.rs:995`, `ui/session.rs:8339` | kept |
| Automatic session archive (`SessionRetentionEnabled`) | Session List | `session_retention_enabled` | `rsid session/retention.rs:49,65` | kept |
| Archive idle hours (`SessionRetentionWindowHours`) | Session List | `session_retention_window_hours` | `rsid session/retention.rs:73` | kept |
| Show system events (`ShowSystemEvents`) | Transcript Defaults | TUI state | `app/sessions.rs:114`, `app/hierarchy_effect.rs:289` | kept |
| Show thinking events (`ShowThinkingEvents`) | Transcript Defaults | TUI state | `app/sessions.rs:115`, `app/hierarchy_effect.rs:290` | kept |
| Hide tool results (`HideToolResults`) | Transcript Defaults | TUI state | `app/sessions.rs:116`, `app/hierarchy_effect.rs:291` | kept |
| Submit on Enter (`SubmitOnEnter`) | Input & Prompts | TUI state | `input_surface.rs:351`, `input_surface/standard.rs:143` | kept |
| Auto-open question panel (`AutoOpenQuestionPanel`) | Input & Prompts | TUI state | `app/polling.rs:961` | kept |
| Prompt compiler (`PromptCompiler`) | Input & Prompts | TUI state | `prompt_processor.rs` (Ctrl+P compile path) | kept |
| Editing mode (`EditingMode`) | Editing Mode | `editing_mode` | `editing_mode_prompt.rs:282` (every input surface) | kept |
| Default model (`DefaultModel`) | Model Roles | TUI state | `app.selected_model` → launch paths (`app/session_actions.rs`) | kept |
| Title model (`TitleModel`) | Model Roles | `title_model_local`, `title_model_provider`, `title_model_base_url`, `title_model_fallback` | `rsid session/lifecycle.rs:1634`, `session/launch.rs:8710` | kept |
| Prompt compiler model (`PromptCompilerModel`) | Model Roles | `prompt_compile_model_local`, `prompt_compile_model_provider`, `prompt_compile_model_base_url` | `rsid prompt_compile/engine.rs:102,105` | kept |
| Memory model (`MemoryModel`) | Model Roles | `memory_model_local`, `memory_model_fallback`, `memory_model_fallback_provider`, `memory_model_fallback_base_url` | `rsid observation/extractor.rs:217-260`, `session/summarizer.rs:242` | kept |
| Dream model (`DreamModel`) | Model Roles | `dream_model`, `dream_model_provider`, `dream_model_base_url` | `rsid dreamer/scheduler.rs:571`, `main.rs:1041` | kept |
| Stall classifier model (`ClassifierModel`) | Model Roles | `stall_classifier_model` | `rsid stall_classifier/scheduler.rs:326` | kept |
| API providers (`ApiProviders`) | API Providers | TUI state | `widget/model_dropdown.rs` (custom provider entries) | kept |
| System prompt preset (`SystemPromptPreset`) | System Prompt | `system_prompt_preset` | `app/session_actions.rs:292,451,507` (launch prompt) | kept |
| Model control mode (`ModelControlMode`) | Model Control | DaemonState | `rsid model_control` admission (every model call) | kept |
| Emergency stop (`EmergencyStop`) | Model Control | DaemonState | `rsid model_control` stop-all verb | kept |
| Orchestration max child effort (`MaxChildEffort`) | Model Control | `orchestration_max_child_effort` | `rsid-store store/model_control.rs:4868` (spawn ceiling) | kept |
| Budget policies (`BudgetPolicies`) | Budgets | DaemonState | `rsid model_control` budget policies (admission) | kept |
| Usage and telemetry (`UsageStats`) | Usage | DaemonState | `rsid model_control` usage statistics | kept |
| Stop-all and cancel (`UsageActions`) | Usage | DaemonState | `rsid model_control` cancel/stop verbs | kept |
| Provider credential slots (`ProviderCredentialSlots`) | Provider Keys | DaemonState | `rsid-store vault/mod.rs` key vault | kept |
| Cloud spend (`CloudSpendStatus`) | Cloud Spend | DaemonState | `rsid cloud_spend.rs` report | kept |
| OpenRouter spend ceiling (`OpenRouterSpendStatus`) | Cloud Spend | DaemonState | `rsid openrouter_spend.rs` state (`GetDaemonConfig` `openrouter_spend_status`) | kept |
| Cloud spend stop line (USD) (`CloudSpendStopLine`) | Cloud Spend | `cloud_spend_stop_line_usd` | caps file (`rsid rpc/settings.rs`, `main.rs:485`) → `rsi-cloud spend` | kept |
| Cloud spend daily cap (USD) (`CloudSpendDailyCap`) | Cloud Spend | `cloud_spend_daily_cap_usd` | caps file (`rsid rpc/settings.rs`, `main.rs:485`) → `rsi-cloud spend` | kept |
| Retry on failure (`RetryOnFailure`) | Retries & Recovery | `retry_enabled` | `rsid session/retry_policy.rs:20`, `session/lifecycle.rs:3019` | kept |
| Retry max backoff (`RetryMaxBackoff`) | Retries & Recovery | `retry_max_backoff_ms` | `rsid session/lifecycle.rs:6127`, `monitor.rs:5013` | kept |
| Retry on stall (`RetryOnStall`) | Retries & Recovery | `retry_on_stall` | `rsid stall_detector.rs:51` | kept |
| Reconciliation loop (`ReconciliationLoop`) | Retries & Recovery | `reconciliation_enabled` | `rsid reconciliation.rs:552` | kept |
| Context rotation (`ContextRotation`) | Context Rotation | `context_rotation_enabled` | `rsid session/context_cap.rs:1328`, `rotation.rs` | kept |
| Context rotation threshold (global) (`ContextRotationGlobalPct`) | Context Rotation | `context_rotation_global_pct` | `rsid session/monitor.rs:1338` via `context_rotation_threshold_pct` | kept |
| Context rotation threshold (Claude Code) (`ContextRotationClaudePct`) | Context Rotation | `context_rotation_claude_pct` | `rsid session/monitor.rs:1338` via `context_rotation_threshold_pct` | kept |
| Context rotation threshold (Codex) (`ContextRotationCodexPct`) | Context Rotation | `context_rotation_codex_pct` | `rsid session/monitor.rs:1338` via `context_rotation_threshold_pct` | kept |
| Coordinator context cap (0 off, 1-100 = % of window) (`CoordinatorContextCap`) | Context Rotation | `coordinator_context_cap_tokens` | `rsid session/context_cap.rs` | kept |
| Manager wake-turn cap (per hour, 0 off) (`ManagerMaxTurnsPerHour`) | Context Rotation | `manager_max_turns_per_hour` | `rsid scheduler/turn_budget.rs` | kept |
| Worker context cap (0 off, 1-100 = % of window) (`WorkerContextCap`) | Context Rotation | `worker_context_cap_tokens` | `rsid session/context_cap.rs` | kept |
| Stall detection (`StallDetection`) | Stall Detection | `stall_detection_enabled` | `rsid stall_detector.rs:225` | kept |
| Stall classifier (`StallClassifier`) | Stall Detection | `stall_classifier_enabled` | `rsid stall_detector.rs:267`, `stall_classifier/scheduler.rs:323` | kept |
| Classifier idle threshold (Claude) (`ClassifierIdleClaude`) | Stall Detection | `stall_classifier_idle_secs` | `rsid stall_detector.rs:109` | kept |
| Classifier idle threshold (Codex) (`ClassifierIdleCodex`) | Stall Detection | `stall_classifier_idle_secs_codex` | `rsid stall_detector.rs:111` | kept |
| Classifier cooldown (`ClassifierCooldown`) | Stall Detection | `stall_classifier_cooldown_secs` | `rsid stall_detector.rs:114` | kept |
| Classifier max per session (`ClassifierMaxPerSession`) | Stall Detection | `stall_classifier_max_per_session` | `rsid stall_detector.rs:117` | kept |
| Classifier confidence floor (`ClassifierConfidenceFloor`) | Stall Detection | `stall_classifier_confidence_floor` | `rsid stall_classifier/scheduler.rs:350` | kept |
| Memory system (`MemorySystem`) | Memory & Dreaming | `memory_enabled` | `rsid memory/sync.rs:96`, `observation/extractor.rs` | kept |
| Dream consolidation (`DreamConsolidation`) | Memory & Dreaming | `dream_enabled` | `rsid dreamer/scheduler.rs` (boot + live gate) | kept |
| Observation threshold (`ObservationThreshold`) | Memory & Dreaming | `dream_observation_threshold` | `rsid dreamer/scheduler.rs:383` | kept |
| Dream cooldown (`DreamCooldown`) | Memory & Dreaming | `dream_cooldown_secs` | `rsid dreamer/scheduler.rs:385` | kept |
| Dream idle wait (`DreamIdle`) | Memory & Dreaming | `dream_idle_secs` | `rsid dreamer/scheduler.rs:601` | kept |
| Dialectic engine (`DialecticEngine`) | Memory & Dreaming | `dialectic_enabled` | `rsid rpc.rs:324` (read at boot) | kept |
| Background queue (`BackgroundQueue`) | Scheduling | `queue_enabled` | `rsid main.rs:592` via `Config::queue_config` (read at boot) | kept |
| Durable topology executor (`TopologyExecutor`) | Scheduling | `topology_executor_enabled` | `rsid session/graph_executions.rs:164`, `topology/agent.rs:1900` | kept |
| Topology build nodes (`TopologyBuildNodes`) | Scheduling | `topology_max_concurrent_build_nodes` | `rsid session/graph_executions.rs:970` | kept |
| Topology bulk fan-out on OpenRouter (0 off) (`TopologyBulkFanout`) | Scheduling | `topology_bulk_fanout_min_openrouter` | `rsid topology/agent.rs:1036`, `session/topology_agent_verbs.rs:223` | kept |
| Hold program wakes while children run (`ProgramHoldWhileChildren`) | Scheduling | `program_hold_while_children_run` | `rsid-store store/child_autonomy.rs:91` → `scheduler/mod.rs:262` | kept |
| Child keep-alive valve (`ChildKeepaliveEnabled`) | Scheduling | `child_keepalive_enabled` | `rsid-store store/child_autonomy.rs:93` → `scheduler/child_gate.rs:38` | kept |
| Child keep-alive window (s) (`ChildKeepaliveWindow`) | Scheduling | `child_keepalive_window_secs` | `rsid-store store/child_autonomy.rs:94` | kept |
| Follow agent-created projects (`FollowAgentCreatedProjects`) | Scheduling | `follow_agent_created_projects` | `app/agent_projects.rs:21,63` | kept |
| Recursive DAG recovery controls (`DagRecoveryControls`) | Recursive DAG | `recursive_dag_recovery_controls_enabled` | `rsid rpc/recursive_control.rs:926` | kept |
| Recursive DAG scheduler controls (`DagSchedulerControls`) | Recursive DAG | `recursive_dag_scheduler_controls_enabled` | `rsid rpc/recursive_control.rs:940` | kept |
| Recursive DAG cancellation controls (`DagCancellationControls`) | Recursive DAG | `recursive_dag_cancellation_controls_enabled` | `rsid rpc/recursive_control.rs:1005` | kept |
| Recursive DAG live scheduler (`DagLiveSchedulerControl`) | Recursive DAG | `recursive_dag_live_scheduler_control_enabled` | `rsid rpc/recursive_control.rs:954` | kept |
| Recursive DAG run lease TTL (`DagRunLeaseTtl`) | Recursive DAG | `recursive_dag_run_lease_ttl_ms` | `rsid rpc/recursive_control.rs:453,518` | kept |
| Recursive DAG max concurrent graphs (`DagMaxConcurrentGraphs`) | Recursive DAG | `recursive_dag_max_concurrent_graphs` | `rsid rpc/recursive_control.rs:463,528` | kept |
| Graph overlay: render recursive origin (`GvRenderRecursiveOrigin`) | Recursive DAG | `gv_render_recursive_origin` | `rsid rpc/recursive_control.rs:968`; `rsi-common rpc.rs:1613` caps | kept |
| Graph overlay: info dashboard (`GvInfoDashboard`) | Recursive DAG | `gv_info_dashboard` | `rsi-common rpc.rs:1617` caps; `ui/overlay/graph.rs:67` | kept |
| Schema-validated topology handoffs (`StructuredHandoffDefault`) | Recursive DAG | `structured_handoff_default` | `rsid topology/executor.rs` `structured_handoff_node` | kept |
| Codegraph indexing (`CodegraphIndexing`) | Code Intelligence | `codegraph_indexing_enabled` | `rsid main.rs:520` index manager, `rpc/codegraph.rs:50` | kept |
| Codex sandbox (`CodexSandbox`) | Provider Isolation | `codex_sandbox_mode` | `rsid codex.rs:452` | kept |
| Codex live model catalog TTL (s) (`CodexLiveCatalogTtl`) | Provider Isolation | `codex_live_catalog_ttl_secs` | `rsid codex_live_catalog.rs` `live_view` | kept |
| Codex plan-window poll (s) (`CodexRateLimitPoll`) | Provider Isolation | `codex_rate_limit_poll_secs` | `rsid codex_rate_limits.rs` `run_poll_loop` | kept |
| Provider lifecycle hooks (`ProviderLifecycleHooks`) | Context Rotation | `provider_lifecycle_hooks` | `rsid claude.rs` `ClaudeClient::lifecycle_hooks`, `codex.rs` `codex_session_hook_overrides_with_lifecycle` | kept |
| Provider skill delivery (`ProviderSkillDelivery`) | Provider Skills | `provider_skill_delivery` | `rsid claude.rs` `ClaudeClient::skill_delivery_explicit`, `codex.rs` `build_cmd`, `skill_delivery.rs` | kept |
| Provider CLI contract canary (`ProviderContractCanary`) | Provider Isolation | `provider_contract_canary` | `rsid provider_contract.rs` `ProviderContractService::check_enabled` | kept |
| Refuse Codex launch when custody hook is untrusted (`CodexRefuseUntrustedCustodyHook`) | Provider Isolation | `codex_refuse_untrusted_custody_hook` | `rsid session/provider_spawn.rs` `codex_custody_refusal` | kept |
| Claude project config isolation (`ClaudeConfigIsolation`) | Provider Isolation | `claude_config_isolation` | `rsid claude.rs:667` | kept |
| Codex user config isolation (`CodexConfigIsolation`) | Provider Isolation | `codex_config_isolation` | `rsid codex.rs` `CodexClient::config_isolation` | kept |
| Provider launch overrides (`ProviderLaunchOverrides`) | Provider Isolation | `provider_launch_overrides` | `rsid claude.rs` `launch_inner`, `rsid codex.rs` `build_cmd_with_custom_provider` (hub #2765) | kept |
| Claude subagent model (`ClaudeSubagentModel`) | Subagent Governance | `claude_subagent_model` | `rsid claude.rs` `claude_subagent_env` | kept |
| Claude max concurrent subagents (`ClaudeMaxConcurrentSubagents`) | Subagent Governance | `claude_max_concurrent_subagents` | `rsid claude.rs` `claude_subagent_env` | kept |
| Codex subagent model (`CodexSubagentModel`) | Subagent Governance | `codex_subagent_model` | `rsid codex.rs` `codex_subagent_overrides` | kept |
| Codex max concurrent subagents (`CodexMaxConcurrentSubagents`) | Subagent Governance | `codex_max_concurrent_subagents` | `rsid codex.rs` `codex_subagent_overrides` | kept |
| Claude fallback models (`ClaudeFallbackModels`) | Provider Isolation | `claude_fallback_models` | `rsid claude.rs` `claude_fallback_chain_for` | kept |
| Claude spend cap per session (USD) (`ClaudeMaxBudgetUsd`) | Provider Isolation | `claude_max_budget_usd` | `rsid claude.rs` `ClaudeClient::launch_inner` | kept |
| OpenRouter spend cap (USD) (`OpenRouterSpendCap`) | Cloud Spend | `openrouter_spend_cap_usd` | `rsid openrouter_spend.rs` (`freeze_launch_identity`, `spawn_provider_process`, `SpendGuardedControl`) | kept |
| OpenRouter spend warn line (%) (`OpenRouterSpendWarnPct`) | Cloud Spend | `openrouter_spend_warn_pct` | `rsid session/manager_coordinator.rs` `reconcile_openrouter_spend_notices` | kept |
| Vault: legacy env fallback (`VaultEnvCompat`) | Provider Isolation | `vault.env_compat` | `rsid-store vault/mod.rs:447` | kept |
| Vault: check TTL (`VaultCheckTtl`) | Provider Isolation | `vault.check_ttl_secs` | `rsid-store vault/mod.rs:882` | kept |
| Provider profile (`ProviderProfile`) | API Routes | `provider_profile` | `rsid-store config.rs` `launch_model_refusal` (every launch) | kept |
| OpenRouter engine (`OpenRouterRoute`) | API Routes | `api_route.openrouter` | `rsid session/provider_spawn.rs:256`, `session/queries.rs:1291` | kept |
| Bedrock engine (`BedrockRoute`) | API Routes | `api_route.bedrock` | `rsid session/harness/mod.rs:236` provider backend | kept |
| OpenRouter Codex fallback (`ApiRouteFallbackOpenRouter`) | API Routes | `api_route.fallback.openrouter` | `rsid session/provider_spawn.rs` OpenRouter arm | kept |
| Bedrock Codex fallback (`ApiRouteFallbackBedrock`) | API Routes | `api_route.fallback.bedrock` (legacy alias `api_route.fallback`) | `rsid session/provider_spawn.rs` Bedrock arm | kept |
| OpenRouter context budget (0 off) (`OpenRouterContextBudget`) | API Routes | `openrouter_context_budget_tokens` | `rsid codex.rs:564`, `session/harness/mod.rs:347` | kept |
| Harness web access (`HarnessWebAccess`) | Harness Tools | `harness_web_access` | `rsid-store config.rs` `harness_tool_policy_defaults` → `session/harness/tools/policy.rs` | kept |
| Harness network egress (`HarnessEgressMode`) | Harness Tools | `harness_egress_mode` | same policy → `session/harness/egress.rs:141` | kept |
| Harness context editing (`HarnessContextEditing`) | Harness Tools | `harness_context_editing` | same policy → harness context editing | kept |
| Harness search call cap (`HarnessSearchCap`) | Harness Tools | `harness_max_search_calls` | same policy → `session/harness/tools/policy.rs:156` | kept |
| Harness fetch call cap (`HarnessFetchCap`) | Harness Tools | `harness_max_fetch_calls` | same policy → `policy.rs:164` | kept |
| Harness completion gates (`HarnessCompletionGates`) | Harness Tools | `completion_gates_enabled` | `rsid session/harness/mod.rs:352` | kept |
| Harness tool output cap (bytes) (`HarnessOutputCap`) | Harness Tools | `harness_max_result_bytes` | same policy → `policy.rs:148` | kept |
| Harness web cost cap (micro-USD) (`HarnessWebCostCap`) | Harness Tools | `harness_max_web_cost_usd_micros` | same policy → `policy.rs:171` | kept |
| MCP deferred tool threshold (`McpDeferredToolThreshold`) | Harness Tools | `mcp.deferred_tool_threshold` | `rsid session/harness/mod.rs:78,329` | kept |
| Harness iterations per turn (`HarnessMaxIterations`) | Harness Tools | `harness_max_iterations_per_turn` | `rsid session/harness/mod.rs:74` | kept |
| Claude hooks (`ClaudeHooks`) | Claude Hooks | `~/.claude/settings.json` | `~/.claude/settings.json` (read by Claude Code; `claude_config.rs`) | kept |
| Claude skills (`ClaudeSkills`) | Claude Skills | `~/.claude/skills/` | `~/.claude/skills/` (read by Claude Code) | kept |
| rsid MemoryHigh (MiB) (`RsidScopeMemoryHigh`) | Resource Limits | `rsid_scope_memory_high_mib` | snapshot env → `rsi main.rs` (systemd unit generator), `scripts/install-release.sh:461` | kept |
| rsid MemoryMax (MiB) (`RsidScopeMemoryMax`) | Resource Limits | `rsid_scope_memory_max_mib` | same snapshot → systemd `MemoryMax` | kept |
| rsid MemorySwapMax (MiB) (`RsidScopeMemorySwapMax`) | Resource Limits | `rsid_scope_memory_swap_max_mib` | same snapshot → systemd `MemorySwapMax` | kept |
| rsid CPUWeight (`RsidScopeCpuWeight`) | Resource Limits | `rsid_scope_cpu_weight` | same snapshot → systemd `CPUWeight` | kept |
| Worker slice MemoryHigh (MiB) (`WorkerScopeMemoryHigh`) | Resource Limits | `worker_scope_memory_high_mib` | same snapshot; `rsid process_scope.rs:122` (worker scopes) | kept |
| Worker slice MemoryMax (MiB) (`WorkerScopeMemoryMax`) | Resource Limits | `worker_scope_memory_max_mib` | same snapshot; `rsid process_scope.rs:122` | kept |
| Worker slice MemorySwapMax (MiB) (`WorkerScopeMemorySwapMax`) | Resource Limits | `worker_scope_memory_swap_max_mib` | `rsid process_scope.rs:122` | kept |
| Worker slice CPUWeight (`WorkerScopeCpuWeight`) | Resource Limits | `worker_scope_cpu_weight` | `rsid process_scope.rs:123` | kept |
| Worker CPU quota (`WorkerCpuQuotaPercent`) | Resource Limits | `worker_cpu_quota_percent` | `rsid process_scope.rs:72-75` via `rsid-core worker_cpu_quota` | kept |
| Hub build CPU budget (`HubBuildCpuPercent`) | Resource Limits | `hub_build_cpu_percent` | `rsid build_budget.rs:40` (rewrites `rsi-build.slice`) | kept |
| Completed transcript cache (bytes) (`CompletedTranscriptCache`) | Resource Limits | `completed_transcript_cache_max_bytes` | `rsid session/queries.rs:754,919` | kept |
| Host load limit for new launches (`HostLoadAdmissionThreshold`) | Build Governor | `host_load_admission_threshold` | `rsid host_load.rs:203` | kept |
| Worker build reserve (GiB) (`WorkerBuildReserveGb`) | Build Governor | `worker_build_reserve_gb` | `rsid host_load.rs` → `host_pressure::launch_hold_now` at every admission | kept |
| CPU andon: CPU-minutes per tree (`CpuAndonCpuMinutes`) | Build Governor | `cpu_andon_cpu_minutes` | `rsid cpu_andon.rs:385` | kept |
| CPU andon: host load (`CpuAndonHostLoad`) | Build Governor | `cpu_andon_host_load` | `rsid cpu_andon.rs:386` | kept |
| Build slots (`GovernorBuildSlots`) | Build Governor | `governor_build_slots` | `rsid governor.rs` via `governor_policy()` | kept |
| Lander slots (`GovernorLanderSlots`) | Build Governor | `governor_lander_slots` | `rsid governor.rs` via `governor_policy()` | kept |
| Check slots (`GovernorCheckSlots`) | Build Governor | `governor_check_slots` | `rsid governor.rs` via `governor_policy()` | kept |
| Governor max load (0 auto) (`GovernorMaxLoad`) | Build Governor | `governor_max_load` | `rsid governor.rs` via `governor_policy()` | kept |
| Governor min free disk (GB) (`GovernorMinFreeDisk`) | Build Governor | `governor_min_free_disk_gb` | `rsid governor.rs:348,651` | kept |
| Governor min available memory (GB) (`GovernorMinAvailMem`) | Build Governor | `governor_min_avail_mem_gb` | `rsid governor.rs:664` | kept |
| Governor max workers-slice memory (GB) (`GovernorMaxWorkersSlice`) | Build Governor | `governor_max_workers_slice_gb` | `rsid governor.rs:671` | kept |
| Rolling merge queue (`RollingQueueEnabled`) | Queue & Deploys | `rolling_queue_enabled` | `rsid rolling_queue.rs:3929` | kept |
| Merge queue batch size (`RollingQueueBatchSize`) | Queue & Deploys | `rolling_queue_batch_size` | `rsid rolling_queue.rs:3932` | kept |
| Merge queue gate timeout (min) (`RollingQueueGateTimeout`) | Queue & Deploys | `rolling_queue_gate_timeout_mins` | `rsid rolling_queue.rs:3935` | kept |
| Merge queue full-QA interval (min) (`RollingQueueFullQaInterval`) | Queue & Deploys | `rolling_queue_full_qa_interval_mins` | `rsid rolling_queue/bounded_qa.rs:194` | kept |
| QA target size cap (GiB) (`QaTargetMaxGib`) | Queue & Deploys | `qa_target_max_gib` | `rsid agent_jobs.rs` `prepare_build_environment_in` via `rsid-core qa_target_cap` and `qa_target_trim::enforce_cap` | kept |
| Job thermal hold (C) (`JobThermalCeiling`) | Queue & Deploys | `job_thermal_ceiling_c` | `rsid agent_jobs.rs` `submit_with_hold` and `start_queued_jobs` via `rsid-core thermal_guard` | kept |
| Health alert: hub disk used (%) (`HealthDiskUsedPct`) | Scheduling | `health_disk_used_pct` | `rsid scheduler/wake_when.rs` `gate` via `rsid-core health_thresholds` | kept |
| Health alert: CPU hot (C) (`HealthThermal`) | Scheduling | `health_thermal_c` | `rsid scheduler/wake_when.rs` `gate` via `rsid-core health_thresholds` | kept |
| Health alert: provider window used (%) (`HealthProviderWindowUsedPct`) | Scheduling | `health_provider_window_used_pct` | `rsid health_anomaly/provider_windows.rs` via `rsid-core health_thresholds` | Live at the next health evaluation; operator RPC and TUI/desktop cycle (0 off, 1-100, default 70) |
| Health alert: work stuck (min) (`HealthStuckMinutes`) | Scheduling | `health_stuck_minutes` | `rsid scheduler/wake_when.rs` `gate` via `rsid-core health_thresholds` | kept |
| Agent test job timeout (min) (`JobTestTimeout`) | Queue & Deploys | `job_test_timeout_mins` | `rsid session/agent_jobs_verb.rs:458` | kept |
| Detach Claude turns (`TurnDetachEnabled`) | Queue & Deploys | `turn_detach_enabled` | `rsid claude/turn_spool.rs:33`, `claude.rs:1328` | kept |
| Hold new work while a deploy waits (`DeployDrainEnabled`) | Queue & Deploys | `deploy_drain_enabled` | `rsid deploy.rs:948,970` | kept |
| Deploy hold limit (s) (`DeployDrainHold`) | Queue & Deploys | `deploy_drain_hold_secs` | `rsid deploy.rs:949,971` | kept |
| Deploy restarts per hour (`DeployRestartsPerHour`) | Queue & Deploys | `deploy_restarts_per_hour` | `rsid deploy.rs:41` (+ supervisor snapshot) | kept |
| Shutdown grace (s) (`SupervisorShutdownGrace`) | Queue & Deploys | `supervisor_shutdown_grace_secs` | `scripts/rsid-supervisor.sh` (supervisor snapshot line 2, written by `rsid deploy.rs` `write_supervisor_restart_budget_snapshot`) | kept |
| Source-worktree settlement (`SourceWorktreeSettlement`) | Sandbox Storage | DaemonState | `overlay/cohort_settlement.rs` audit overlay | kept |
| Legacy scratch adoption (`LegacyScratchAdoption`) | Sandbox Storage | DaemonState | `overlay` legacy-scratch adoption list | kept |
| Maximum sandbox roots (`SandboxMaxSourceRoots`) | Sandbox Storage | `sandbox_max_source_roots` | `rsid session/launch.rs:10363` (sandbox allocation admission) | kept |
| Minimum free space (GiB) (`SandboxMinFreeGib`) | Sandbox Storage | `sandbox_min_free_gib` | `rsid session/launch.rs:10368` (sandbox allocation floor) | kept |
| Automatic source settlement (`SourceWorktreeAutoSettlement`) | Sandbox Storage | `source_worktree_auto_settlement_enabled` | `rsid session/cohort_settlement.rs:361` | kept |
| Disk pressure free space (GB) (`DiskPressureMinFreeGb`) | Sandbox Storage | `disk_pressure_min_free_gb` | `rsid session/cohort_settlement.rs:371` | kept |
| Purge archived sandboxes (`ArchivedSandboxPurge`) | Sandbox Storage | `archived_sandbox_purge_enabled` | `rsid session/sandbox_purge.rs:171` | kept |
| Resolve errored sessions (`ErrorDispositionEnabled`) | Cold Storage | `error_disposition_enabled` | `rsid session/error_disposition.rs::run_error_disposition_loop` | kept |
| Error grace (minutes) (`ErrorDispositionGraceMinutes`) | Cold Storage | `error_disposition_grace_minutes` | `rsid session/error_disposition.rs::error_disposition_pass` | kept |
| Error continuations per day (`ErrorDispositionContinuationBudget`) | Cold Storage | `error_disposition_continuation_budget` | `rsid session/error_disposition.rs::error_disposition_pass` | kept |
| Cold-storage archive (`ColdArchiveEnabled`) | Cold Storage | `cold_archive_enabled` | `rsid session/cold_archive.rs` | kept |
| Cold-storage age (days) (`ColdArchiveAfterDays`) | Cold Storage | `cold_archive_after_days` | `rsid session/cold_archive.rs` | kept |
| Cold-storage directory (`ColdArchiveDir`) | Cold Storage | `cold_archive_dir` | `rsid session/cold_archive.rs` (`cold_archive_settings`), `overlay/daemon_text_editor.rs` | kept |
| Cold-storage status (`ColdArchiveStatus`) | Cold Storage | DaemonState | `GetColdArchiveStatus` read (`cold_storage.rs`) | kept |
| Cold-storage last run (`ColdArchiveLastRun`) | Cold Storage | DaemonState | `GetColdArchiveStatus` read (`cold_storage.rs`) | kept |
| Run cold-storage pass (`ColdArchiveRunNow`) | Cold Storage | DaemonState | `RunColdArchiveNow` (`rsid rpc/cold_archive.rs`) | kept |
| Stored tool-result cap (KiB) (`ToolResultStorageCap`) | Sandbox Storage | `tool_result_storage_cap_kib` | `rsid-store store/events.rs` `stored_tool_result_body` via `rsid-core tool_result_cap` | kept |
| Keep referenced pasted images (days) (`PasteRetentionReferencedDays`) | Sandbox Storage | `paste_retention_referenced_days` | `rsid paste_retention.rs` `run_policy_pass` via `rsid-core paste_retention_setting` | kept |
| Sandbox storage (`SandboxStorageStatus`) | Build Cache | DaemonState | `rsid rpc/storage.rs:199` dry-run report | kept |
| Sandbox cache reclaim (`CacheReclaim`) | Build Cache | `sandbox_build_cache_reclaim_enabled` | `rsid session/launch.rs:10577` (reclaim worker gate), `shared_target_reclaim.rs` | kept |
| Cache reclaim TTL (`CacheReclaimTtl`) | Build Cache | `sandbox_build_cache_reclaim_ttl_secs` | `rsid session/launch.rs:780` (min idle), `:11150` | kept |
| Cache reclaim interval (`CacheReclaimInterval`) | Build Cache | `sandbox_build_cache_reclaim_interval_secs` | `rsid session/launch.rs:10575` | kept |
| Cache pressure high (`CachePressureHigh`) | Build Cache | `sandbox_build_cache_reclaim_high_watermark_pct` | `rsid session/launch.rs:10652` | kept |
| Cache pressure low (`CachePressureLow`) | Build Cache | `sandbox_build_cache_reclaim_low_watermark_pct` | `rsid session/launch.rs:10653` | kept |
| Cache reclaim pass limit (`CacheReclaimPassLimit`) | Build Cache | `sandbox_build_cache_reclaim_max_candidates` | `rsid session/launch.rs:10729` | kept |
| Preview cache reclaim (`PreviewReclaim`) | Build Cache | DaemonState | `GetSandboxStorageStatus` / `RunSandboxBuildCacheReclaim` dry run | kept |
| Reclaim sandbox caches now (`ReclaimNow`) | Build Cache | DaemonState | `RunSandboxBuildCacheReclaim` | kept |
| Warm build pools kept (`TargetPoolKeep`) | Build Cache | `target_pool_keep` | `rsid-store sandbox/execution_scratch.rs:269`, `crates/rsi-repo-guards/src/target_pool_ops.rs:59` | kept |
| Warm build pools (`TargetPoolStatus`) | Build Cache | DaemonState | `GetTargetPoolStatus` report | kept |
| Agent build jobs (`AgentBuildJobs`) | Agent Builds | `agent_build_jobs` | `rsid-store sandbox/execution_scratch.rs:258` | kept |
| Agent line tables (`AgentBuildLineTables`) | Agent Builds | `agent_build_line_tables_only` | `rsid-store sandbox/execution_scratch.rs:259` | kept |
| Worker sccache (`AgentBuildSccache`) | Agent Builds | `agent_build_sccache_enabled` | `rsid-store sandbox/execution_scratch.rs:256` | kept |
| sccache cap (GiB) (`AgentBuildSccacheSize`) | Agent Builds | `agent_build_sccache_cache_gib` | `rsid-store sandbox/execution_scratch.rs:267` | kept |
| Machine build slots (`AgentBuildSlots`) | Agent Builds | `agent_build_slots` | `rsid-store sandbox/execution_scratch.rs:262` | kept |
| Signal bridge (`SignalBridge`) | Message Bridges | `signal.toml` | `signal.toml` read by the bridge process at its start | kept |
| iMessage bridge (`ImessageBridge`) | Message Bridges | `imessage.toml` | `imessage.toml` read by the bridge process at its start | kept |
| Satellite registry (`SatelliteRegistry`) | Satellites | DaemonState | `overlay` satellite registry (hub RPCs) | kept |
| Satellite polling (`SatellitePolling`) | Satellites | `satellite_polling_enabled` | `rsid satellite/hub.rs:175` | kept |
| Remote access (`RemoteAccess`) | Remote | DaemonState | `overlay` remote settings (`GetRemoteAccess` RPC family) | kept |
| MCP servers (`McpServerConfigurations`) | MCP Servers | DaemonState | `overlay/mcp_server_form.rs` (ListMcpServers family) | kept |
| Max retries (`MaxRetries`) | was Retries & Recovery | `retry_max_default` | `rsid session/retry_policy.rs:32` loads it and discards it (`kind_default_max_retries` is 0 for every kind); only `config.rs` seeds `retry_enabled` from it at boot | removed (row; key stays as the boot seed of the retry kill-switch) |
| Merge queue speculation depth (`RollingQueueSpeculationDepth`) | was Queue & Deploys | `rolling_queue_speculation_depth` | none: only echoed by `GetRollingQueue`; the queue loop (`rolling_queue.rs:3929-3940`) reads batch size and gate timeout, never speculation depth | removed (row, key, status field, catalog entry) |

### Earlier per-row live-apply audit (Issue #1200)

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
| Retry max backoff (`RetryMaxBackoff`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `retry_max_backoff_ms`. |
| Retry on stall (`RetryOnStall`) | Daemon restart | Live | Subscribe even when disabled at boot; read retry_on_stall before each stall-event retry. Per-session retry policy remains enforced. Owner: `retry_on_stall`. |
| Reconciliation loop (`ReconciliationLoop`) | Daemon restart | Live | Loop starts once and reads runtime enable each tick; disabled passes maintain watchdog heartbeat without touching sessions. Owner: `reconciliation_enabled`. |
| Context rotation (`ContextRotation`) | New launches live; resumed sessions need daemon restart | New launches live; resumed sessions need daemon restart | New launches read runtime switch; restore/continuation paths retain SessionManager boot flag and active rotation state. Updating an in-progress handoff is outside a small safe reload. Owner: `context_rotation_enabled`. |
| Context rotation threshold (global) (`ContextRotationGlobalPct`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `context_rotation_global_pct`. |
| Context rotation threshold (Claude Code) (`ContextRotationClaudePct`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `context_rotation_claude_pct`. |
| Context rotation threshold (Codex) (`ContextRotationCodexPct`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `context_rotation_codex_pct`. |
| Coordinator context cap (0 off, 1-100 = % of window) (`CoordinatorContextCap`) | Live | Live | Runtime values are read at the corresponding retry/context decision. Owner: `coordinator_context_cap_tokens`. |
| Manager wake-turn cap (per hour, 0 off) (`ManagerMaxTurnsPerHour`) | Live | Live | The scheduler reads the persisted cap at each manager-notice delivery (#4021); a seat over the cap has routine-only deliveries held and coalesced into the next free turn. Applies to the next delivery. Owner: `manager_max_turns_per_hour`. |
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
| Schema-validated topology handoffs (`StructuredHandoffDefault`) | Live | Live | Read when the executor opens a node's handoff-format continuation; a turn already launched keeps the flags it launched with. Owner: `structured_handoff_default`. |
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
| Deploy restarts per hour (`DeployRestartsPerHour`) | Live | Live | The deploy runner copies the limit into the deploy service at every 5 s poll; each request and quiet-point restart checks it. Owner: `deploy_restarts_per_hour`. |
| Shutdown grace (s) (`SupervisorShutdownGrace`) | Live | Live | `UpdateDaemonConfig` rewrites the supervisor snapshot (line 2) atomically; the supervisor re-reads it when a daemon shutdown begins. Owner: `supervisor_shutdown_grace_secs`. |
| Host load limit for new launches (`HostLoadAdmissionThreshold`) | Live | Live | The manager-action claim loop (every pass, at most 10 s apart) and each topology node launch read the threshold at the moment they decide; raising or zeroing it releases held launches on the next pass, lowering it holds queued ones. Owner: `host_load_admission_threshold`. |
| Worker build reserve (GiB) (`WorkerBuildReserveGb`) | Live | Live | Each manager-create and topology admission reads the reserve above the governor floor; lowering it or setting 0 releases held launches on the next admission pass. Existing build/test jobs keep their governor floor. Owner: `worker_build_reserve_gb`. |
| Worker CPU quota (`WorkerCpuQuotaPercent`) | Live | Next launch (no daemon/TUI restart) | `ScopedWorkerCommand` reads the value when it creates each worker scope and passes `CPUQuota=<n>%` (0 omits it). Running scopes keep their quota; new workers and tool scopes use the new value. Linux only. Owner: `worker_cpu_quota_percent`. |
| Hub build CPU budget (`HubBuildCpuPercent`) | Live | Live | `UpdateDaemonConfig` rewrites the `rsi-build.slice` runtime properties (`CPUQuota=<threads x n>%`, `CPUWeight=20`) through `systemctl --user set-property --runtime`, so units already running in the slice are bounded at once; the daemon re-applies it at start and before each build unit or lander it launches, and job launchers read the value to cap their own per-job ceilings. Linux only: elsewhere the response reports `unavailable`. Owner: `hub_build_cpu_percent`. |
| Merge queue batch size (`RollingQueueBatchSize`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `rolling_queue_batch_size`. |
| Merge queue gate timeout (min) (`RollingQueueGateTimeout`) | Live | Live | Read when the queue claims its next batch (the batch gate deadline); a batch already gating keeps the deadline it was claimed with. Owner: `rolling_queue_gate_timeout_mins`. |
| Merge queue full-QA interval (min) (`RollingQueueFullQaInterval`) | Live | Live | Read by the full-library QA task before each dispatch; a run already started keeps going. Owner: `rolling_queue_full_qa_interval_mins`. |
| QA target size cap (GiB) (`QaTargetMaxGib`) | Live | Live | `UpdateDaemonConfig` publishes the value into `rsid_core::qa_target_cap`; the job launcher reads it when a full-library QA job is prepared (#2509) and empties a shared QA target above it. 0 turns the cap off. Owner: `qa_target_max_gib`. |
| Job thermal hold (C) (`JobThermalCeiling`) | Live | Live | `UpdateDaemonConfig` publishes the value into `rsid_core::thermal_guard`; `submit_with_hold` and `start_queued_jobs` read it (and the host's hwmon CPU temperature) at each build/test job start (#2524). A running job is not stopped; 0 turns the hold off. Owner: `job_thermal_ceiling_c`. |
| Health alert: hub disk used (%) (`HealthDiskUsedPct`) | Live | Live | `UpdateDaemonConfig` publishes the value into `rsid_core::health_thresholds`; the scheduler's `health_anomaly` wake evaluation reads it every minute (#2747). Owner: `health_disk_used_pct`. |
| Health alert: CPU hot (C) (`HealthThermal`) | Live | Live | `UpdateDaemonConfig` publishes the value into `rsid_core::health_thresholds`; the scheduler's `health_anomaly` wake evaluation reads it every minute (#2747). Owner: `health_thermal_c`. |
| Health alert: work stuck (min) (`HealthStuckMinutes`) | Live | Live | `UpdateDaemonConfig` publishes the value into `rsid_core::health_thresholds`; the scheduler's `health_anomaly` wake evaluation reads it every minute (#2747). Owner: `health_stuck_minutes`. |
| Agent test job timeout (min) (`JobTestTimeout`) | Live | Live | Read by `AgentSubmitJob` at each submit and stamped into the test job's params (#1337); a running job keeps the timeout it was submitted with. Owner: `job_test_timeout_mins`. |
| CPU andon: CPU-minutes per tree (`CpuAndonCpuMinutes`) | Live | Live | Read by `rsid::cpu_andon` at every one-minute sample (#1337); 0 turns the trigger off. Owner: `cpu_andon_cpu_minutes`. |
| CPU andon: host load (`CpuAndonHostLoad`) | Live | Live | Read by `rsid::cpu_andon` at every one-minute sample (#1337); 0 turns the trigger off. Owner: `cpu_andon_host_load`. |
| Hold program wakes while children run (`ProgramHoldWhileChildren`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `program_hold_while_children_run`. |
| Child keep-alive valve (`ChildKeepaliveEnabled`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `child_keepalive_enabled`. |
| Child keep-alive window (s) (`ChildKeepaliveWindow`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `child_keepalive_window_secs`. |
| Build slots (`GovernorBuildSlots`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_build_slots`. |
| Lander slots (`GovernorLanderSlots`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_lander_slots`. |
| Check slots (`GovernorCheckSlots`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_check_slots`. |
| Governor max load (0 auto) (`GovernorMaxLoad`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_max_load`. |
| Governor min free disk (GB) (`GovernorMinFreeDisk`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_min_free_disk_gb`. |
| Governor min available memory (GB) (`GovernorMinAvailMem`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_min_avail_mem_gb`. |
| Governor max workers-slice memory (GB) (`GovernorMaxWorkersSlice`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `governor_max_workers_slice_gb`. |
| Harness web access (`HarnessWebAccess`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_web_access`. |
| Harness network egress (`HarnessEgressMode`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_egress_mode`. |
| Harness web search backend (`HarnessWebSearchBackend`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_web_search_backend`. |
| Harness SearXNG URL (`HarnessSearxngUrl`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_searxng_url`. |
| Benchmark docker hosts (`BenchDockerHosts`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `bench.docker_hosts`. |
| Benchmark jobs enabled (`BenchEnabled`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `bench.enabled`. |
| Benchmark concurrency (`BenchMaxConcurrency`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `bench.max_concurrency`. |
| Benchmark laptop host (`BenchLaptopHost`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `bench.laptop_host`. |
| Harness context editing (`HarnessContextEditing`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_context_editing`. |
| Harness search call cap (`HarnessSearchCap`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_search_calls`. |
| Harness fetch call cap (`HarnessFetchCap`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_fetch_calls`. |
| Harness completion gates (`HarnessCompletionGates`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `completion_gates_enabled`. |
| Harness tool output cap (bytes) (`HarnessOutputCap`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_result_bytes`. |
| Harness web cost cap (micro-USD) (`HarnessWebCostCap`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `harness_max_web_cost_usd_micros`. |
| MCP deferred tool threshold (`McpDeferredToolThreshold`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Next-launch policy is deliberately stable for an already running session/build. Owner: `mcp.deferred_tool_threshold`. |
| Cloud spend (`CloudSpendStatus`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `cloud spend`. |
| OpenRouter spend ceiling (`OpenRouterSpendStatus`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `openrouter spend`. |
| Cloud spend stop line (USD) (`CloudSpendStopLine`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `cloud_spend_stop_line_usd`. |
| Cloud spend daily cap (USD) (`CloudSpendDailyCap`) | Live | Live | Scheduler/admission/request handlers read runtime controls at their next decision; does not retroactively cancel accepted work. Owner: `cloud_spend_daily_cap_usd`. |
| Codegraph indexing (`CodegraphIndexing`) | Live | Live | Indexer uses shared runtime flag and publication gate; acknowledged OFF prevents subsequent publication. Owner: `codegraph_indexing_enabled`. |
| Codex sandbox (`CodexSandbox`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `codex_sandbox_mode`. |
| Codex live model catalog TTL (s) (`CodexLiveCatalogTtl`) | Live | Live | Read at each live-catalog lookup (discovery, the manager preflight and launch admission, #2752); a cached list is judged against the new value at the next lookup. 0 reads only the bundled catalog. Owner: `codex_live_catalog_ttl_secs`. |
| Codex plan-window poll (s) (`CodexRateLimitPoll`) | Live | Live | The poll loop reads the interval at every cycle (#2757): a change applies within one idle tick (15 s), 0 stops probing at once and leaves the stored windows. Owner: `codex_rate_limit_poll_secs`. |
| Provider lifecycle hooks (`ProviderLifecycleHooks`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Read at each Claude and Codex CLI launch (fresh, continue and rotation) when the hook command is built; a running provider process keeps the hooks it launched with. Owner: `provider_lifecycle_hooks`. |
| Provider skill delivery (`ProviderSkillDelivery`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Read at each Claude and Codex CLI launch (fresh, continue and rotation) when the argv is built (`--plugin-dir`, `-c skills.config`); a running provider process keeps the skills it launched with. The monitor reads it once at a Claude init to judge the `plugins` list. Owner: `provider_skill_delivery`. |
| Provider CLI contract canary (`ProviderContractCanary`) | Live | Live | Read at each canary run (daemon start, a CLI version change, an operator refresh); turning it off reports the providers unchecked and never refuses a launch. Owner: `provider_contract_canary`. |
| Refuse Codex launch when custody hook is untrusted (`CodexRefuseUntrustedCustodyHook`) | Live | Live | Read at each Codex CLI launch (fresh, continue and rotation); a running session keeps the hook state it launched with. Owner: `codex_refuse_untrusted_custody_hook`. |
| Claude project config isolation (`ClaudeConfigIsolation`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `claude_config_isolation`. |
| Codex user config isolation (`CodexConfigIsolation`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `codex_config_isolation`. |
| Provider launch overrides (`ProviderLaunchOverrides`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Read at each Claude / Codex CLI launch (every Claude turn, every Codex `exec`/`exec resume`); a running CLI process keeps the options it started with, and each run records what it applied on the session (hub #2765). Owner: `provider_launch_overrides`. |
| Claude subagent model (`ClaudeSubagentModel`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch reads the saved value; existing provider subprocesses keep launch-time settings. Owner: `claude_subagent_model`. |
| Claude max concurrent subagents (`ClaudeMaxConcurrentSubagents`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch reads the saved value; existing provider subprocesses keep launch-time settings. Owner: `claude_max_concurrent_subagents`. |
| Codex subagent model (`CodexSubagentModel`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch reads the saved value; existing provider subprocesses keep launch-time settings. Owner: `codex_subagent_model`. |
| Codex max concurrent subagents (`CodexMaxConcurrentSubagents`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch reads the saved value; existing provider subprocesses keep launch-time settings. Owner: `codex_max_concurrent_subagents`. |
| Claude fallback models (`ClaudeFallbackModels`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `claude_fallback_models`. |
| Claude spend cap per session (USD) (`ClaudeMaxBudgetUsd`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `claude_max_budget_usd`. |
| OpenRouter spend cap (USD) (`OpenRouterSpendCap`) | Live | Live | Read at each OpenRouter-billed launch and before every Harness model call; arming (off to on) takes a fresh probe of the account usage as the baseline. Owner: `openrouter_spend_cap_usd`. |
| OpenRouter spend warn line (%) (`OpenRouterSpendWarnPct`) | Live | Live | Read by the manager pass that sends the one warn notice per arming. Owner: `openrouter_spend_warn_pct`. |
| Vault: legacy env fallback (`VaultEnvCompat`) | Live | Live | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Owner: `vault.env_compat`. |
| Vault: check TTL (`VaultCheckTtl`) | Live | Live | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Owner: `vault.check_ttl_secs`. |
| OpenRouter engine (`OpenRouterRoute`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `api_route.openrouter`. |
| Bedrock engine (`BedrockRoute`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `api_route.bedrock`. |
| Provider profile (`ProviderProfile`) | Live | Live | #1407: `RuntimeConfig::launch_model_refusal` reads the profile at every launch, continuation, rotation and spawn check; the TUI pickers re-filter when the row changes. Running sessions are not stopped. Owner: `provider_profile`. |
| OpenRouter Codex fallback (`ApiRouteFallbackOpenRouter`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `api_route.fallback.openrouter`. |
| Bedrock Codex fallback (`ApiRouteFallbackBedrock`) | Next launch (no daemon/TUI restart) | Next launch (no daemon/TUI restart) | Session launch/credential resolution reads saved policy; existing provider subprocesses keep launch-time settings. Next-launch policy is deliberately stable for an already running session/build. Owner: `api_route.fallback.bedrock` (legacy alias `api_route.fallback`). |
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
| Automatic source settlement (`SourceWorktreeAutoSettlement`) | Live | Live | The local five-minute loop and apply fences read the switch before intent and each removal. Retained startup recovery remains proof-gated. Owner: `source_worktree_auto_settlement_enabled`. |
| Disk pressure free space (GB) (`DiskPressureMinFreeGb`) | Live | Live | Automatic source settlement samples the local sandbox filesystem before intent and each removal; pool trimming reads the same machine-local threshold. Owner: `disk_pressure_min_free_gb`. |
| Source-worktree settlement (`SourceWorktreeSettlement`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `source-worktree settlement`. |
| Legacy scratch adoption (`LegacyScratchAdoption`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `legacy scratch adoption`. |
| Maximum sandbox roots (`SandboxMaxSourceRoots`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_max_source_roots`. |
| Minimum free space (GiB) (`SandboxMinFreeGib`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `sandbox_min_free_gib`. |
| Warm build pools kept (`TargetPoolKeep`) | Live | Live | Read when a seed starts a pool build and on every reclaim pass; pools beyond it are removed, never one a seed is reading. Owner: `target_pool_keep`. |
| Warm build pools (`TargetPoolStatus`) | Live | Live | Status is an immediate RPC read (`GetTargetPoolStatus`). Owner: `target pool`. |
| Purge archived sandboxes (`ArchivedSandboxPurge`) | Live | Live | Storage maintenance/admission reads current runtime policy; status/actions are immediate RPC operations. Owner: `archived_sandbox_purge_enabled`. |
| Resolve errored sessions (`ErrorDispositionEnabled`) | Live | Live | The disposition loop reads the switch every five minutes; a pass already running finishes. Owner: `error_disposition_enabled`. |
| Error grace (minutes) (`ErrorDispositionGraceMinutes`) | Live | Live | Each bounded pass reads its grace cutoff, which is checked again before a disposition. Owner: `error_disposition_grace_minutes`. |
| Error continuations per day (`ErrorDispositionContinuationBudget`) | Live | Live | Each bounded pass reads its continuation budget; durable attempts in the preceding 24 hours count against it. Owner: `error_disposition_continuation_budget`. |
| Cold-storage archive (`ColdArchiveEnabled`) | Live | Live | The cold-archive loop reads the switch at every 10-minute tick; a pass already running finishes. Owner: `cold_archive_enabled`. |
| Cold-storage age (days) (`ColdArchiveAfterDays`) | Live | Live | Read when a pass starts; a pass already running keeps its cutoff. Owner: `cold_archive_after_days`. |
| Cold-storage directory (`ColdArchiveDir`) | Live | Live | Read when a pass starts (and checked against the sandbox roots then); a pass already running keeps its directory, and existing archive files keep their recorded path. Owner: `cold_archive_dir`. |
| Stored tool-result cap (KiB) (`ToolResultStorageCap`) | Live | Live | `UpdateDaemonConfig` publishes the value into `rsid_core::tool_result_cap`; the event writer reads it for every `ToolResult` it stores (#2373), so it applies to results stored after the change and never rewrites a stored row. Owner: `tool_result_storage_cap_kib`. |
| Keep referenced pasted images (days) (`PasteRetentionReferencedDays`) | Live | Live | `UpdateDaemonConfig` publishes the value into `rsid_core::paste_retention_setting`; the paste retention pass reads it at its next pass (daemon start, or after an upload at most hourly, #86). 0 (the default) keeps every referenced image. Owner: `paste_retention_referenced_days`. |
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

## Provider billing (#3533)

| Claude billing (`ClaudeBillingMode`) | Provider Billing | `claude_billing_mode` | Store billing aggregates and admission gates | live on next read; historical estimates classified without rewriting rows |
| Codex billing (`CodexBillingMode`) | Provider Billing | `codex_billing_mode` | Store billing aggregates and admission gates | live on next read; historical estimates classified without rewriting rows |
| CodexAppServer billing (`CodexAppServerBillingMode`) | Provider Billing | `codexappserver_billing_mode` | Store billing aggregates and admission gates | live on next read; historical estimates classified without rewriting rows |
| Pioneer billing (`PioneerBillingMode`) | Provider Billing | `pioneer_billing_mode` | Store billing aggregates and admission gates | live on next read; historical estimates classified without rewriting rows |
| Antigravity billing (`AntigravityBillingMode`) | Provider Billing | `antigravity_billing_mode` | Store billing aggregates and admission gates | live on next read; historical estimates classified without rewriting rows |

## Host tuning, Slice A (#3532)

`:manager tune` opens the local host probe and profile/preset editor. The Resource
Limits tab and command help point to it. The profile is an operator preference,
not a RuntimeConfig field; it uses dedicated `GetHostTuning`,
`ConfigureHostTuning` and `SwitchHostPreset` RPCs. It adds no general settings-page
row or raw daemon setting. The versioned JSON record `host_tuning.v1` in
`daemon_settings` holds the profile, explicit numeric per-key bounds, named
presets and the prior values for undo; no schema migration is needed.

The panel stages reserve/dedicated intent, session-cap guidance, preset values
and their bounds. `n` captures current settings into a new named draft; initial
bounds are exactly those captured values, so widening them is an explicit
operator choice. Tab and j/k select a preset or resource key, Enter edits its
value, m/M edit its minimum/maximum, b edits reserve %, d selects dedicated,
g edits session guidance. w saves the complete draft with a revision fence;
a applies a saved preset; u restores the last switch's prior values; r discards
the draft and refreshes evidence. Profile/reserve never silently change settings.

| Resource key | Consumer / outcome |
|---|---|
| `worker_cpu_quota_percent` | Next worker/tool scope launch; existing scopes retain quota. Linux systemd only. The prior Live label is corrected to NextSpawn. |
| `hub_build_cpu_percent` | Live shared build and parent CPU slice update; report carries actual systemd apply state. Unsupported or failed enforcement is reported as incomplete, even if values were saved. |
| `host_load_admission_threshold` | Live host-load admission for new sessions/topology launches. |
| `governor_build_slots`, `governor_check_slots`, `governor_lander_slots` | Live governor admission; already admitted jobs finish. |
| `governor_min_avail_mem_gb`, `governor_min_free_disk_gb` | Live governor memory/disk floors; macOS daemon admission reads the published build slot/memory policy. |
| `agent_build_jobs`, `agent_build_slots` | Next agent/build launch; existing launch environments stay fixed. |
| `agent_build_sccache_cache_gib` | Next launch's sccache environment. |
| `sandbox_min_free_gib`, `disk_pressure_min_free_gb` | Live sandbox/admission and reclamation floors. |
| `target_pool_keep` | Live pool publication/reclaim pass; active seed readers stay protected. |
| `sandbox_build_cache_reclaim_ttl_secs` | Live cache reclamation pass. |
| Live-session cap | Profile guidance only. Manager-policy `max_active_sessions` remains its own authority grant and is never changed by tuning. |
| scoped_test/test threads | Per-job option and load-adaptive runner policy; no existing persisted daemon setting. Not accepted in presets. |

No supported preset key requires a daemon restart. Apply and undo use the same
validated batch persistence/publication and side-effect path as
`UpdateDaemonConfig`, serialized against ordinary setting updates. The prior
values are stored before effects. Re-applying an already matching active preset
keeps both the revision and undo record. Failed/partial effects retain a pending
undo record; retry re-runs effects even when saved values already match. Receipts
always include actual runtime read-back and apply classes. Operator and agent
undo both honor the currently saved per-key bounds; an operator tightening a
bound may need to widen it to recover an older value.

`AgentSwitchHostPreset` is RPC-only and requires a current Execute, unpaused
project, area or portfolio manager seat. It accepts only a saved name or undo;
it cannot edit profiles, reserves, bounds or presets. Explicit manager-policy
bounds further narrow the tuning bounds. The three operator RPCs remain absent
from AGENT_VERBS, READ_VERBS, native tools and the agent CLI catalog.

The probe uses portable available_parallelism and Unix filesystem free space.
Linux reads memory/load, CPU sensors, and v1/v2 cgroup membership, mount roots and
ancestor quotas; the tightest quota caps effective CPUs. RAM is explicitly host
MemTotal/MemAvailable, not a cgroup allowance. Missing readings carry unknown
reasons; non-Linux memory/cgroup/load/temperature readings are unsupported.
Windows disk probes are unknown (unavailable), with portable CPU parallelism.
Guided value derivation, schedules, accelerator discovery and satellites remain
outside Slice A. No running host settings are changed by this implementation task.
