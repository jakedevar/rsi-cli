// Typed adapter over the manager Tauri commands (src-tauri/src/managers.rs).
// Each call is one operator daemon method; the Rust side validates the shape again.
import { invoke } from './api.js';
import type {
  BudgetSnapshot, FleetOverview, ManagerConfig, PortfolioNode, StatePage, StatePolicy, TreePage, Workspace,
} from './managers-model.js';

type Json = Record<string, unknown>;

export interface ScopePage {
  rows: Array<{ id: string; title: string; kind: string; group_id: string | null; group_title: string | null }>;
  next_after_id: string | null;
}

export const managersApi = {
  tree: (after: string | null, limit: number) => invoke<TreePage>('manager_tree', { after, limit }),
  config: (projectId: string) => invoke<ManagerConfig | null>('harness_manager_get', { projectId }),
  policy: (projectId: string) => invoke<StatePolicy | null>('harness_manager_policy_get', { projectId }),
  scopePage: (projectId: string, afterId: string | null) => invoke<ScopePage>('harness_manager_scope_page', { projectId, afterId }),
  state: (projectId: string, section: string, cursor: string | null, limit: number) =>
    invoke<StatePage>('harness_manager_state', { projectId, section, cursor, limit }),
  configureScope: (request: Json) => invoke<Json>('harness_manager_configure_scope', { request }),
  configurePolicy: (request: Json) => invoke<StatePolicy>('harness_manager_configure_policy', { request }),
  answer: (request: Json) => invoke<Json>('harness_manager_answer_decision', { request }),
  staleDecisions: (projectId: string | null) =>
    invoke<{ older_than_days: number; rows: Array<{ decision: Json; question: string; status: string }>; complete: boolean }>(
      'manager_stale_decisions', { projectId }),
  archiveStale: (request: Json) => invoke<Json>('manager_archive_stale_decisions', { request }),
  globalGrant: () => invoke<Workspace['grant']>('global_manager_get'),
  workspace: () => invoke<Workspace>('global_manager_workspace'),
  budget: (nodeId: string | null) => invoke<BudgetSnapshot>('global_manager_budget', { nodeId }),
  revokeGlobal: (expectedGrantVersion: number, idempotencyKey: string) =>
    invoke<Json>('global_manager_revoke', { expectedGrantVersion, idempotencyKey }),
  fleet: () => invoke<FleetOverview>('fleet_overview'),
  portfolioNodes: (includeRevoked: boolean) => invoke<{ nodes: PortfolioNode[] }>('portfolio_nodes_list', { includeRevoked }),
  portfolioNode: (nodeId: string) => invoke<PortfolioNode | null>('portfolio_node_get', { nodeId }),
  configurePortfolio: (request: Json, confirmCapReductions: boolean) =>
    invoke<PortfolioNode>('portfolio_node_configure', { request, confirmCapReductions }),
  revokePortfolio: (request: Json) => invoke<Json>('portfolio_node_revoke', { request }),
  areaNode: (projectId: string, nodeId: string) => invoke<Json | null>('area_node_get', { projectId, nodeId }),
  revokeArea: (request: Json) => invoke<Json>('area_node_revoke', { request }),
  escalations: (includeClosed: boolean) => invoke<Json>('operator_escalations', { includeClosed }),
  friction: (windowHours: number | null) => invoke<Json>('friction_rollup', { windowHours }),
  // ---- gap sweep G3 (src-tauri/src/manager_ops.rs): forwarders, one daemon method each ----
  configureGlobal: (request: Json, confirm: boolean) => invoke<Json>('global_manager_configure', { request, confirm }),
  configurePortfolioRequest: (request: Json, confirm: boolean) => invoke<PortfolioNode>('portfolio_node_configure_request', { request, confirm }),
  portfolioLaunches: (request: Json, confirm: boolean) => invoke<PortfolioNode>('portfolio_node_launches', { request, confirm }),
  managerNodes: (projectId: string, after: string | null) => invoke<{ rows: Json[]; next_after_node_id: string | null }>('manager_nodes_list', { projectId, after }),
  configureNode: (request: Json) => invoke<Json>('manager_node_configure', { request }),
  restartStatus: () => invoke<Json>('operator_restart_status'),
  restartForce: () => invoke<Json>('operator_restart_force'),
  restartCancel: () => invoke<Json>('operator_restart_cancel'),
  ruleEscalation: (hopId: string, ruling: string, idempotencyKey: string) => invoke<Json>('operator_escalation_rule', { hopId, ruling, idempotencyKey }),
  ackNotice: (messageId: string) => invoke<Json>('operator_notice_ack', { messageId }),
  undelivered: (after: string | null) => invoke<Json>('operator_undelivered', { after }),
  nodeWorkspace: (kind: 'portfolio' | 'project' | 'area', id: string) => invoke<Json>('manager_node_workspace', { kind, id }),
  appointWithPolicy: (request: Json) => invoke<Json>('harness_manager_with_policy', { request }),
  // ---- planners (no daemon): the TUI's request builders over rsi-common ----
  planGlobalAppoint: (current: unknown, nodes: unknown, sessionId: string | null, projectIds: string[], useDefaults: boolean) =>
    invoke<{ request: Json | null; note: string }>('mgr_global_appoint_plan', { current, nodes, sessionId, projectIds, useDefaults }),
  planGlobalSet: (grant: unknown, field: string, value: string) => invoke<{ request: Json; change: string }>('mgr_global_set_plan', { grant, field, value }),
  planGlobalAddProjects: (grant: unknown, projectIds: string[]) => invoke<Json>('mgr_global_add_projects_plan', { grant, projectIds }),
  planGlobalEdit: (grant: unknown, seat: string | null, projectIds: string[], caps: Json | null) =>
    invoke<Json>('mgr_global_edit_plan', { grant, seat, projectIds, caps }),
  planPortfolioAppoint: (nodes: unknown, label: string, seat: string, projectIds: string[], adopt: string[]) =>
    invoke<{ request: Json; note: string | null }>('mgr_portfolio_appoint_plan', { nodes, label, seat, projectIds, adopt }),
  planPortfolioLaunch: (node: unknown, add: boolean, launches: string[]) => invoke<Json>('mgr_portfolio_launch_plan', { node, add, launches }),
  planPortfolioEdit: (node: unknown, seat: string | null, projectIds: string[] | null, caps: Json | null) =>
    invoke<Json>('mgr_portfolio_edit_plan', { node, seat, projectIds, caps }),
  planPortfolioAbove: (node: unknown, seat: string) => invoke<{ request: Json; label: string }>('mgr_portfolio_above_plan', { node, seat }),
  planPortfolioMove: (target: unknown, moved: unknown) => invoke<{ request: Json; added: string[] }>('mgr_portfolio_move_plan', { target, moved }),
  planNodeEdit: (node: unknown, parent: unknown, values: [number, number, number]) => invoke<Json>('mgr_node_edit_plan', { node, parent, values }),
  policyClassify: (policy: Json, scopeMode: string) =>
    invoke<{ preset: string; profile: string | null; suggestions: Array<{ field: string; from: number; to: number }>; root_group_mismatch: boolean; validation_error: string | null }>(
      'mgr_policy_classify', { policy, scopeMode }),
  policyPreset: (policy: Json, preset: string, scopeMode: string, saved: boolean) => invoke<Json>('mgr_policy_preset', { policy, preset, scopeMode, saved }),
  policySuggestions: (policy: Json, scopeMode: string) => invoke<Json>('mgr_policy_suggestions', { policy, scopeMode }),
  policyCheck: (policy: Json) => invoke<Json>('mgr_policy_check', { policy }),
  defaultGrant: () => invoke<{ policy: Json; launches: Json[] }>('mgr_default_grant'),
  planPmAppoint: (projectId: string, sessionId: string, expectedRowVersion: number, expectedPolicyVersion: number, policy: Json | null) =>
    invoke<Json>('mgr_pm_appoint_plan', { projectId, sessionId, expectedRowVersion, expectedPolicyVersion, policy }),
};
