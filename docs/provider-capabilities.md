# Provider capabilities

This file is generated from the daemon's typed provider-capability registry. Edit the registry or checked fixtures, then regenerate this document.

<!-- BEGIN GENERATED: provider-capability-validator -->

## Provenance

- Installed CLI: `codex-cli 0.155.1`
- Semantic fixture: `crates/rsid/tests/fixtures/codex-models-0.155.1.json` (`sha256:a60498398ac4824e943151d11e2479b003eb971b4180fdcd862d49fcdc577321`)
- Schema coverage fixture: `crates/rsid/tests/fixtures/codex-models-0.155.1-schema.json` (39-key catalog)
- Raw installed catalog: `sha256:404c1dd19656240244e0ae74d7e82326bd192ec4c2a0e6f32d130075fa1fb0f4` (acceptance artifact only)
- Required consumer roles: `crates/rsid/provider-capability-required-consumer-roles.txt` (independent, non-generated acceptance anchor)

The semantic fixture is bounded and omits provider prompt payloads. Its digest is intentionally distinct from the raw installed-catalog digest. The schema fixture keeps complete coverage of the 39 installed model fields. RSI preserves every visible installed-catalog entry and places current GPT-6 models first.

## RSI model projection

| Model | Display | Visibility / priority | Efforts (default first) | Default / effective / max context | Modalities | Tool mode / search |
| --- | --- | --- | --- | --- | --- | --- |
| `gpt-6-astra` | GPT-6-Astra | list / 1 | low, medium, high, xhigh, max, ultra (low) | 272000 / 258400 / 872000 | text, image | code_mode_only / text_and_image |

## Separate capacity sources

| Detail | Value | Source and authority |
| --- | --- | --- |
| `gpt-6-astra` advertised context | 1050000 | [Official documentation](https://developers.openai.com/api/docs/models/gpt-6-astra); descriptive, never an active CLI denominator by itself |
| `gpt-6-astra` maximum output | 128000 | [Official documentation](https://developers.openai.com/api/docs/models/gpt-6-astra); separate from installed catalog context |
| Session compaction limit | runtime/configured when present | The installed 0.155.1 catalog omits a context-compaction threshold; `comp_hash` and `truncation_policy` are preserved metadata, not token capacity |

## Catalog field classification

| Field | Category | Handling | Detail |
| --- | --- | --- | --- |
| `additional_speed_tiers` | service | ignored with reason | service-tier presentation is outside context capability resolution |
| `apply_patch_tool_type` | tools | consumed | CodexCatalogToolCapabilities.apply_patch_tool_type |
| `availability_nux` | presentation | ignored with reason | provider onboarding state is not a daemon capability |
| `base_instructions` | prompt | ignored with reason | large provider prompt payload is private to the installed CLI |
| `comp_hash` | compaction | consumed | CodexCatalogCompactionMetadata.comp_hash |
| `context_window` | context | consumed | ContextCapacity.provider_default_tokens |
| `default_reasoning_level` | effort | consumed | CodexCatalogModel.default_reasoning_level |
| `default_reasoning_summary` | presentation | ignored with reason | reasoning-summary presentation does not change capacity |
| `default_verbosity` | presentation | ignored with reason | response verbosity does not change context capacity |
| `description` | identity | consumed | CodexCatalogModel.description |
| `display_name` | identity | consumed | CodexCatalogModel.display_name |
| `effective_context_window_percent` | context | consumed | ContextCapacity.effective_percent |
| `experimental_supported_tools` | tools | consumed | CodexCatalogToolCapabilities.experimental_supported_tools |
| `include_apps_usage_instructions` | prompt | ignored with reason | installed CLI owns app-instruction assembly |
| `include_plugin_usage_instructions` | prompt | ignored with reason | installed CLI owns plugin-instruction assembly |
| `include_skills_usage_instructions` | prompt | ignored with reason | installed CLI owns skill-instruction assembly |
| `input_modalities` | modalities | consumed | CodexCatalogToolCapabilities.input_modalities |
| `max_context_window` | context | consumed | ContextCapacity.provider_max_tokens |
| `model_messages` | prompt | ignored with reason | provider-owned message payload is intentionally absent from fixtures |
| `multi_agent_reasoning_effort` | routing | ignored with reason | installed CLI owns its delegated-worker reasoning policy |
| `multi_agent_version` | routing | ignored with reason | installed CLI owns its multi-agent protocol selection |
| `node_repl_auto_review_required` | tools | ignored with reason | installed CLI owns Node REPL review policy |
| `node_repl_disabled` | tools | ignored with reason | installed CLI owns Node REPL enablement |
| `priority` | visibility | consumed | CodexCatalogModel.priority |
| `service_tiers` | service | ignored with reason | billing and latency tiers are not context-capacity evidence |
| `shell_type` | tools | consumed | CodexCatalogToolCapabilities.shell_type |
| `slug` | identity | consumed | CodexCatalogModel.slug |
| `support_verbosity` | presentation | ignored with reason | verbosity support is not used by daemon context resolution |
| `supported_in_api` | visibility | consumed | CodexCatalogModel.supported_in_api |
| `supported_reasoning_levels` | effort | consumed | CodexCatalogModel.supported_reasoning_levels |
| `supports_experimental_context` | context | ignored with reason | installed CLI owns experimental-context behavior |
| `supports_image_detail_original` | modalities | consumed | CodexCatalogToolCapabilities.supports_image_detail_original |
| `supports_search_tool` | tools | consumed | CodexCatalogToolCapabilities.supports_search_tool |
| `tool_mode` | tools | consumed | CodexCatalogToolCapabilities.tool_mode |
| `truncation_policy` | compaction | consumed | CodexCatalogCompactionMetadata.truncation_policy |
| `upgrade` | presentation | ignored with reason | provider upgrade messaging is not capability evidence |
| `use_responses_lite` | transport | ignored with reason | installed CLI owns its Responses transport selection |
| `visibility` | visibility | consumed | CodexCatalogModel.visibility |
| `web_search_tool_type` | tools | consumed | CodexCatalogToolCapabilities.web_search_tool_type |

## Closed consumer inventory

| Role | Path | Symbol | Required dataflow |
| --- | --- | --- | --- |
| startup-catalog-refresh | `crates/rsid/src/session/mod.rs` | `SessionManager::refresh_codex_catalog_at_startup` | `call:provider_capabilities` → `call:refresh_catalog` |
| launch | `crates/rsid/src/session/launch.rs` | `build_starting_session` | `call:resolve_fresh_context_budget` → `field:context_window, field:resolved_context_budget` |
| restoration-reopen | `crates/rsid/src/session/lifecycle.rs` | `SessionManager::continue_session_with_delivery` | `call:resolve_new_incarnation_context_budget` → `call:compare_and_update_session_model, call:install_context_budget` |
| live-monitor | `crates/rsid/src/session/monitor.rs` | `persist_runtime_context_observation` | `call:resolve_runtime_context_budget` → `call:compare_and_update_session_model, call:install_context_budget, return` |
| memory-flush | `crates/rsid/src/session/monitor.rs` | `memory_flush_turn_candidate` | `call:context_budget` → `call:should_run_memory_flush, field:active_tokens` |
| rotation | `crates/rsid/src/session/rotation.rs` | `SessionManager::rotate_completed_session` | `call:resolve_new_incarnation_context_budget` → `field:context_window, field:resolved_context_budget` |
| context-injection | `crates/rsid/src/session/launch.rs` | `SessionManager::launch_session_with_retry_admission` | `call:context_injection_allowance` → `call:assemble` |
| harness-full-window-compaction | `crates/rsid/src/session/harness/mod.rs` | `HarnessClient::launch` | `parameter:resolved_context_budget` → `call:run_harness_loop` |
| persistence | `crates/rsid/src/store/sessions.rs` | `persisted_context_budget` | `parameter:resolved` → `field:source, field:source_version, field:source_digest, field:observed_at` |
| rpc-session | `crates/rsid/src/session/queries.rs` | `rehydrate_context_budget_projection` | `call:rehydrate_resolved_context_budget` → `assignment:session.resolved_context_budget` |
| bus-publication | `crates/rsid/src/monitor.rs` | `publish_context_usage` | `parameter:resolved_context_budget` → `field:context_window, field:resolved_context_budget` |
| polling | `crates/rsi/src/app/polling.rs` | `App::apply_push_event` | `field:parsed.resolved_context_budget` → `assignment:state.session.resolved_context_budget` |
| f3-detail | `crates/rsi/src/ui/overlay/session_info.rs` | `session_info_lines` | `call:detail_rows` → `call:field_line` |
| wide-inspector-detail | `crates/rsi/src/ui/session.rs` | `render_inspector_context` | `call:detail_rows` → `call:push_inspector_section` |
| detail-header-context | `crates/rsi/src/ui/status.rs` | `render_context_percent_segment_for` | `call:compact_label` → `call:styled` |
| wide-inspector-compact | `crates/rsi/src/ui/session.rs` | `render_inspector_runtime` | `field:runtime.context` → `call:compact_label` |
| session-list-compact-row | `crates/rsi/src/types/row.rs` | `compute_session_row_for_state_with_focus` | `call:compute_context_budget_view` → `call:compact_label` |
| tests | `crates/rsid/src/provider_capabilities.rs` | `real_codex_0_155_1_fixture_preserves_capacity_reasoning_and_projection` | `call:fixture_snapshot` → `macro:assert_eq` |

<!-- END GENERATED: provider-capability-validator -->
