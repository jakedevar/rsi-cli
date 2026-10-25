// Read-only invoke bridge for the permission-free desktop smoke (hub #3000).
//
// The frontend reaches the backend only through `globalThis.__TAURI__.core.invoke(cmd, args)`
// (src/api.ts) and `__TAURI__.event.listen` (src/live.ts, src/menus.ts). The smoke serves
// desktop/dist to a headless browser with a shim for those two, and the shim forwards each
// invoke to `createBridge(...).invoke`. The bridge:
//   * maps a Tauri command to the SAME rsid JSON-RPC method the Rust handler calls
//     (src-tauri/src/*.rs), via READ_COMMANDS below;
//   * refuses every command that is not in that table, and every method that is not in
//     READ_METHODS, and records the refusal. A mutation is never forwarded;
//   * never carries a credential: no session token is added (the desktop is authenticated
//     by owning the socket, daemon.rs) and any `secret`-like argument is refused.
import { assertIsolatedSocket } from './smoke-isolated.mjs';
import { defaultSocket, rpcCall } from './smoke-rpc.mjs';

export { defaultSocket, rpcCall };

const uuid = (name, v) => {
  if (typeof v !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(v)) {
    throw new Error(`${name} must be a UUID`);
  }
  return v;
};
const none = () => null;
const escapeHtml = (t) => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');
const SECRET_FIELD = /secret|password|passphrase|credential|api[_.-]?key|private[_.-]?key|bearer|token|(^|[_.-])key($|[_.-])/i;
const URL_USERINFO = /^([A-Za-z][A-Za-z0-9+.-]*:\/\/)[^/?#]*@/;

/** What the webview may see of GetDaemonConfig: secret-named fields and URL user-info masked (a superset of settings.rs redact_config). */
export function redactConfig(v, depth = 0) {
  if (depth > 6 || v === null) return v;
  if (Array.isArray(v)) return v.map((x) => redactConfig(x, depth + 1));
  if (typeof v === 'string') return v.replace(URL_USERINFO, '$1••••@');
  if (typeof v !== 'object') return v;
  return Object.fromEntries(Object.entries(v).map(([k, x]) => [
    k, SECRET_FIELD.test(k) && x !== null && x !== '' ? '••••' : redactConfig(x, depth + 1),
  ]));
}

const refuse = (why) => { throw new Error(why); };
/** managers.rs `SECTIONS`: `ManagerInspectSectionV2` in its serde spelling. */
const MANAGER_SECTIONS = ['overview', 'workers', 'work', 'requests', 'decisions', 'topology', 'resources', 'actions', 'events', 'archive', 'health',
  'migration_allocations', 'satellites', 'friction', 'rulings'];
const boundedInt = (name, v, min, max) => {
  if (!Number.isInteger(v) || v < min || v > max) throw new Error(`${name} must be ${min}..=${max}`);
  return v;
};
/** managers.rs `cursor`: a short text without NUL. */
const cursorText = (name, v, max) => {
  if (typeof v !== 'string' || !v || v.length > max || v.includes('\0')) throw new Error(`${name} is malformed`);
  return v;
};
const history = (a) => {
  const p = { project_id: uuid('project_id', a?.projectId), issue_id: uuid('issue_id', a?.issueId) };
  if (a?.afterSequence != null) p.after_sequence = a.afterSequence;
  if (a?.limit != null) p.limit = a.limit;
  return p;
};
/** A project-file request: a closed key set, exactly one of project_id / session_id (files.rs `root`). */
const fileRequest = (allowed, extra = (o) => o) => (a) => {
  const o = a?.request;
  if (!o || typeof o !== 'object') throw new Error('request must be an object');
  for (const k of Object.keys(o)) if (!allowed.includes(k)) throw new Error(`unknown field ${k}`);
  const out = {};
  const hasP = o.project_id != null;
  const hasS = o.session_id != null;
  if (hasP === hasS) throw new Error('name exactly one of project_id and session_id');
  if (hasP) out.project_id = uuid('project_id', o.project_id); else out.session_id = uuid('session_id', o.session_id);
  for (const k of allowed) if (k !== 'project_id' && k !== 'session_id' && o[k] != null) out[k] = o[k];
  if (typeof out.path !== 'string') out.path = '';
  return extra(out);
};
/** MoveProjectEntry / CopyProjectEntry take `from` and `to`, not `path`. */
const transferShape = (o) => {
  delete o.path;
  for (const k of ['from', 'to']) if (typeof o[k] !== 'string' || !o[k].trim()) throw new Error(`${k} names no file`);
  return o;
};
const CREDENTIAL_KEYS = ['slot', 'state', 'fingerprint', 'set_at', 'rotated_from_fingerprint', 'cleared_at', 'generation', 'route', 'cli_exposure', 'last_cli_exposure_at'];
const CHECK_KEYS = ['generation', 'at', 'class', 'http_status', 'credit_remaining', 'detail_code', 'fingerprint'];
const pick = (o, keys) => Object.fromEntries(keys.filter((k) => o && Object.hasOwn(o, k)).map((k) => [k, o[k]]));
/** admin.rs redact_credential: one slot's metadata, allowlisted (no field can carry key bytes). */
export function redactCredential(meta) {
  const out = pick(meta, CREDENTIAL_KEYS);
  if (meta?.check && typeof meta.check === 'object') out.check = pick(meta.check, CHECK_KEYS);
  return out;
}
/** admin.rs redact_provider_credentials. */
export function redactCredentialList(list) {
  return { ...pick(list, ['env_compat', 'check_ttl_secs']), credentials: (Array.isArray(list?.credentials) ? list.credentials : []).map(redactCredential) };
}

const WORKFLOW_STAGES = ['Research', 'ResearchComplete', 'Planning', 'PlanComplete', 'Implementing', 'ImplementComplete', 'Complete'];
const checkDefinition = (d) => {
  if (!d || typeof d !== 'object' || !Array.isArray(d.nodes) || !Array.isArray(d.edges)) throw new Error('the workflow definition needs nodes and edges lists');
  if (JSON.stringify(d).length > 1_000_000) throw new Error('the workflow definition is too large');
};
/** graphs.rs upsert_params: the row fields the TUI's build_workflow_document sets, and a definition with nodes and edges. */
const upsertParams = (doc) => {
  const row = doc?.workflow;
  if (!row || typeof row !== 'object') throw new Error('document.workflow must be an object');
  checkDefinition(doc.definition);
  if (!WORKFLOW_STAGES.includes(row.stage)) throw new Error(`workflow.stage must be one of ${WORKFLOW_STAGES.join(', ')}`);
  if (typeof row.title !== 'string' || !row.title.trim()) throw new Error('workflow.title is empty');
  for (const k of ['created_at', 'updated_at']) if (typeof row[k] !== 'string') throw new Error(`workflow.${k} must be text`);
  return {
    document: {
      workflow: {
        id: uuid('workflow.id', row.id), title: row.title, stage: row.stage, artifact_path: row.artifact_path ?? null,
        project_id: row.project_id ? uuid('workflow.project_id', row.project_id) : null, created_at: row.created_at, updated_at: row.updated_at,
      },
      definition: doc.definition,
    },
  };
};
/**
 * Stand-in for rsi_graph::validate_executable_workflow, the TUI's run preflight, which runs in the Tauri backend (graphs.rs
 * validate) and cannot be reached from Node: the structural errors only. rsid's own ExecuteWorkflow validation stays the
 * authority and answers any workflow this lets through.
 */
export function validateWorkflowLocal(workflow) {
  const diagnostics = [];
  const push = (code, path, message, nodeId = null) => diagnostics.push({ code, severity: 'error', subject: { path, node_id: nodeId }, message });
  const ids = new Set();
  (workflow?.nodes ?? []).forEach((n, i) => {
    const id = String(n?.id ?? '').trim();
    if (!id) push('empty_node_id', `/nodes/${i}/id`, 'node id cannot be empty');
    else if (ids.has(id)) push('duplicate_node_id', `/nodes/${i}/id`, `duplicate node id '${id}'`, id);
    else ids.add(id);
    if (n?.type !== 'action') push('unsupported_node_kind', `/nodes/${i}/type`, `node '${n?.id}' uses unsupported executable type '${n?.type}'`, n?.id ?? null);
  });
  (workflow?.edges ?? []).forEach((e, i) => {
    for (const end of ['source', 'target']) if (!ids.has(e?.[end])) push('unknown_edge_node', `/edges/${i}/${end}`, `edge ${end} '${e?.[end]}' is not a node`);
  });
  return { executable: diagnostics.length === 0, diagnostics };
}

/** Tauri command -> { method, params(args) }. Only reads. Mirrors src-tauri/src (see each module). */
export const READ_COMMANDS = {
  // src-tauri daemon_info never throws: an unreachable rsid answers { connected: false, error }, which the first-run guidance shows.
  daemon_info: {
    method: 'GetHealthStatus', params: none,
    wrap: (health) => ({ socket: 'daemon.sock', connected: true, health }),
    fallback: (e) => ({ socket: 'daemon.sock', connected: false, error: String(e.message) }),
  },
  // The newer-build notice reads this on a 30 s cadence (local #322): build_sha and local_build.
  get_daemon_build: { method: 'GetDaemonInfo', params: () => ({}) },
  list_projects: { method: 'ListProjects', params: none },
  list_sessions: { method: 'ListSessions', params: none },
  list_archived_sessions: { method: 'ListArchivedSessions', params: (a) => (a?.projectId ? { project_id: a.projectId } : null) },
  get_session: { method: 'GetSession', params: (a) => ({ session_id: uuid('session_id', a?.sessionId) }) },
  list_labels: { method: 'ListLabels', params: none },
  // Reads the app makes the moment a view opens (local #298). Left off this table they were treated as writes and answered
  // null, so the view failed with "Cannot read properties of null" instead of showing itself. Each mirrors src-tauri/src.
  list_mcp_servers: { method: 'ListMcpServers', params: none },
  list_satellite_peers: { method: 'ListSatellitePeers', params: none },
  get_satellite_scope_status: { method: 'GetSatelliteScopeStatus', params: none },
  get_satellite_inbound_policy: { method: 'GetSatelliteInboundPolicy', params: none },
  list_hub_satellite_sessions: {
    method: 'ListHubSatelliteSessions',
    params: (a) => ({ peer_id: uuid('peer_id', a?.peerId), offset: boundedInt('offset', a?.offset ?? 0, 0, 1_000_000), limit: 30 }),
  },
  list_legacy_scratch: { method: 'ListLegacyScratch', params: none },
  list_source_worktree_cohorts: { method: 'ListSourceWorktreeCohorts', params: () => ({}) },
  list_source_worktree_settlement_runs: { method: 'ListSourceWorktreeSettlementRuns', params: () => ({}) },
  get_source_worktree_settlement_run: { method: 'GetSourceWorktreeSettlementRun', params: (a) => ({ run_id: uuid('run_id', a?.runId) }) },
  get_drain_restart_status: { method: 'GetDrainRestartStatus', params: () => ({}) },
  list_skills: { method: 'ListSkills', params: () => ({ user: true, max_entries: 256 }) },
  get_skill: { method: 'GetSkill', params: (a) => ({ user: true, name: cursorText('name', a?.name, 128) }) },
  list_prompt_files: { method: 'ListPromptFiles', params: () => ({}) },
  get_prompt_file: { method: 'GetPromptFile', params: (a) => ({ name: cursorText('name', a?.name, 128) }) },
  fleet_overview: { method: 'GetFleetOverview', params: () => ({}) },
  portfolio_nodes_list: { method: 'ListPortfolioNodes', params: (a) => ({ include_revoked: !!a?.includeRevoked }) },
  manager_nodes_list: {
    method: 'ListManagerNodes',
    params: (a) => ({ project_id: uuid('project_id', a?.projectId), after_node_id: a?.after ? uuid('after_node_id', a.after) : null, limit: 50 }),
  },
  friction_rollup: { method: 'ListFrictionRollup', params: (a) => (a?.windowHours == null ? {} : { window_hours: boundedInt('window_hours', a.windowHours, 1, 720) }) },
  operator_escalations: { method: 'ListOperatorEscalations', params: (a) => ({ include_closed: !!a?.includeClosed, undelivered_after: null }) },
  operator_restart_status: { method: 'GetOperatorRestart', params: () => ({}) },
  get_entity_card: {
    method: 'GetEntityCard',
    params: (a) => ({ entity_type: cursorText('entity_type', a?.entityType, 64), entity_id: cursorText('entity_id', a?.entityId, 128) }),
  },
  remote_get_status: { method: 'RemoteGetStatus', params: none },
  list_scheduled_jobs: {
    method: 'ListScheduledJobs',
    params: (a) => ({ include_history: !!a?.includeHistory }),
    wrap: (page) => (Array.isArray(page) ? { jobs: page, next_cursor: null, include_history: false } : page),
  },
  list_scheduled_job_holds: { method: 'ListScheduledJobHolds', params: none },
  operator_todo_list: { method: 'OperatorTodoList', params: (a) => ({ closed_since: a?.closedSince || null }) },
  get_daemon_config: { method: 'GetDaemonConfig', params: () => ({}), wrap: (c) => redactConfig(c) },
  // Local #125: the Settings status rows (settings_state.rs, commandmode.rs); each only reads.
  model_control_overview: { method: 'GetModelControlStatus', params: () => ({ recent_limit: 8 }) },
  usage_stats: { method: 'GetUsageStats', params: () => ({ project_id: null }) },
  cold_archive_status: { method: 'GetColdArchiveStatus', params: none },
  sandbox_storage_status: { method: 'GetSandboxStorageStatus', params: none },
  target_pool_status: { method: 'GetTargetPoolStatus', params: none },
  list_operator_messages: { method: 'ListOperatorMessages', params: (a) => ({ session_id: uuid('session_id', a?.sessionId) }) },
  get_operator_pause: { method: 'GetOperatorPause', params: (a) => ({ session_id: uuid('session_id', a?.sessionId) }) },
  list_issues_page: { method: 'ListIssuesPage', params: (a) => ({ ...a?.request, project_id: uuid('project_id', a?.request?.project_id) }) },
  list_issue_dependencies: {
    method: 'ListIssueDependencies',
    params: (a) => ({ ...a?.page, project_id: uuid('project_id', a?.projectId), issue_id: uuid('issue_id', a?.issueId) }),
  },
  list_claude_hooks: { method: 'ListClaudeHooks', params: () => ({}) },
  list_custom_providers: { method: 'ListCustomProviders', params: () => ({}) },
  list_custom_provider_routes: { method: 'ListCustomProviderRoutes', params: () => ({}) },
  list_message_bridges: { method: 'ListMessageBridges', params: () => ({}) },
  get_issue: { method: 'GetIssueInProject', params: (a) => ({ project_id: uuid('project_id', a?.projectId), issue_id: uuid('issue_id', a?.issueId) }) },
  list_issue_events: { method: 'ListIssueEventsV2', params: (a) => history(a) },
  // Local #128: the graph review's reads (src-tauri/src/graphs.rs); each only reads.
  graph_capabilities: { method: 'GetDaemonCapabilities', params: none },
  graph_list_workflows: { method: 'ListWorkflows', params: (a) => (a?.projectId ? { project_id: uuid('project_id', a.projectId) } : {}) },
  graph_get_workflow: { method: 'GetWorkflowDefinition', params: (a) => ({ workflow_id: uuid('workflow_id', a?.workflowId) }) },
  graph_get_execution: { method: 'GetWorkflowExecution', params: (a) => ({ execution_id: uuid('execution_id', a?.executionId) }) },
  // The recursive DAG browser's reads (src-tauri/src/graphs.rs dag_* and graph_list_recursive); its controls are writes and stay out.
  graph_list_recursive: { method: 'ListRecursiveTaskGraphs', params: (a) => ({ project_id: a?.projectId ? uuid('project_id', a.projectId) : null, include_quarantined: !!a?.includeQuarantined }) },
  dag_get_graph: { method: 'GetRecursiveTaskGraph', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId) }) },
  dag_operational_status: { method: 'GetRecursiveDagOperationalStatus', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId) }) },
  dag_scheduler_runs: { method: 'ListRecursiveSchedulerRuns', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId), limit: 12, include_terminal: true }) },
  dag_scheduler_run: { method: 'GetRecursiveSchedulerRun', params: (a) => ({ run_id: uuid('run_id', a?.runId) }) },
  dag_run_events: { method: 'ListRecursiveSchedulerRunEvents', params: (a) => ({ run_id: uuid('run_id', a?.runId), limit: 80 }) },
  dag_cancellations: { method: 'ListRecursiveCancellationRequests', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId), limit: 24 }) },
  dag_recovery_status: { method: 'GetRecursiveRecoveryStatus', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId) }) },
  dag_stale_heartbeats: { method: 'ListStaleRecursiveLiveAttemptHeartbeats', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId), limit: 24 }) },
  dag_live_recovery: { method: 'GetRecursiveLiveRecoveryStatus', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId), include_recovery_pending: true, include_deferred_graph: true }) },
  dag_live_attempts: { method: 'ListRecursiveLiveAttempts', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId), include_terminal: true, include_status: true, limit: 24 }) },
  dag_validations: { method: 'ListRecursiveLiveOutputValidationResults', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId), include_issues: true, limit: 24 }) },
  dag_artifacts: { method: 'ListRecursiveExecutionArtifactSummaries', params: (a) => ({ graph_id: uuid('graph_id', a?.graphId), limit: 24, include_total: false, ...(a?.cursor ? { cursor: String(a.cursor) } : {}) }) },
  list_issue_handoffs: { method: 'ListIssueHandoffs', params: (a) => history(a) },
  list_provider_credentials: { method: 'ListProviderCredentials', params: none, wrap: (l) => redactCredentialList(l) },
  get_provider_login_state: { method: 'GetProviderLoginState', params: (a) => (a?.refresh ? refuse('a refresh runs the provider CLI') : null) },
  get_provider_contract_state: { method: 'GetProviderContractState', params: (a) => (a?.refresh ? refuse('a refresh runs the provider CLI') : null) },
  list_project_directory: { method: 'ListProjectDirectory', params: fileRequest(['project_id', 'session_id', 'path', 'max_entries']) },
  list_project_files: { method: 'ListProjectFiles', params: fileRequest(['project_id', 'session_id', 'path', 'max_paths']) },
  read_project_file: { method: 'ReadProjectFile', params: fileRequest(['project_id', 'session_id', 'path', 'max_bytes', 'start_line', 'end_line']) },
  get_project_file_git_gutter: { method: 'GetProjectFileGitGutter', params: fileRequest(['project_id', 'session_id', 'path']) },
  list_slash_commands: { method: 'ListSlashCommands', params: (a) => {
    const o = a?.request;
    if (!o || typeof o !== 'object') throw new Error('request must be an object');
    for (const k of Object.keys(o)) if (!['project_id', 'session_id'].includes(k)) throw new Error(`unknown field ${k}`);
    if ((o.project_id != null) === (o.session_id != null)) throw new Error('name exactly one of project_id and session_id');
    return { ...(o.project_id != null ? { project_id: uuid('project_id', o.project_id) } : { session_id: uuid('session_id', o.session_id) }), max_entries: 500 };
  } },
  get_conversation: {
    method: 'GetConversation',
    params: (a) => {
      const p = { session_id: uuid('session_id', a?.sessionId) };
      if (a?.sinceSequence != null) p.since_sequence = a.sinceSequence;
      return p;
    },
  },
  // The manager surfaces (src-tauri/src/managers.rs builders): the tree, the board, the policy and scope forms, the global
  // manager grant, workspace and budget panel, and the dashboard. Each only reads; every write sits behind WRITE_COMMANDS.
  manager_tree: { method: 'GetManagerTree', params: (a) => {
    const limit = boundedInt('limit', a?.limit, 1, 200);
    return a?.after == null ? { limit } : { limit, after: cursorText('after', a.after, 128) };
  } },
  harness_manager_get: { method: 'GetHarnessManager', params: (a) => ({ project_id: uuid('project_id', a?.projectId) }) },
  harness_manager_policy_get: { method: 'GetHarnessManagerPolicy', params: (a) => ({ project_id: uuid('project_id', a?.projectId) }) },
  // The Tauri handler pins the page bound (SCOPE_MAX_LIMIT) itself.
  harness_manager_scope_page: { method: 'ListHarnessManagerScope', params: (a) => {
    const p = { project_id: uuid('project_id', a?.projectId), limit: 64 };
    if (a?.afterId != null) p.after_id = uuid('after_id', a.afterId);
    return p;
  } },
  harness_manager_state: { method: 'GetHarnessManagerState', params: (a) => {
    if (!MANAGER_SECTIONS.includes(a?.section)) throw new Error(`section must be one of ${MANAGER_SECTIONS.join(', ')}`);
    const query = { section: a.section, limit: boundedInt('limit', a?.limit, 1, 128) };
    if (a?.cursor != null) query.cursor = cursorText('cursor', a.cursor, 512);
    return { project_id: uuid('project_id', a?.projectId), query };
  } },
  global_manager_get: { method: 'GetGlobalManager', params: () => ({}) },
  global_manager_workspace: { method: 'GetGlobalManagerWorkspace', params: () => ({}) },
  global_manager_budget: { method: 'GetGlobalManagerBudget', params: (a) => (a?.nodeId == null ? {} : { node_id: uuid('node_id', a.nodeId) }) },
  global_dashboard: { method: 'GetGlobalDashboard', params: (a) => {
    if (!['1h', '24h', '7d'].includes(a?.window)) throw new Error('window must be one of 1h, 24h, 7d');
    return { window: a.window };
  } },
  // The Trash browser and the session model picker (lifecycle.rs, detail.rs).
  list_deleted_sessions: { method: 'ListDeletedSessions', params: (a) => (a?.projectId ? { project_id: uuid('project_id', a.projectId) } : {}) },
  get_session_model_switch_options: { method: 'GetSessionModelSwitchOptions', params: (a) => ({ session_id: uuid('session_id', a?.sessionId) }) },
  // budgets.rs reads the model-control status with no recent rows and keeps only the mode and the policies.
  budget_policies: {
    method: 'GetModelControlStatus', params: () => ({ recent_limit: 0 }),
    wrap: (status) => ({ mode: status?.mode, policies: status?.policies ?? [] }),
  },
};

// ---------- isolated-write mode (hub #3105) ----------
// Mutating commands. They are reachable ONLY through `createBridge({ isolated })`, and `isolated` must name a
// socket of an rsid this very run started (smoke-isolated.mjs). The builders mirror the Rust handlers in
// src-tauri/src (issues.rs, admin.rs, settings.rs) so the daemon sees the params the real app would send.
const str = (name, v, max) => {
  if (typeof v !== 'string' || v.length > max) throw new Error(`${name} must be a string of at most ${max} characters`);
  return v;
};
const nonblank = (name, v, max) => { const s = str(name, v, max); if (!s.trim()) throw new Error(`${name} must not be blank`); return s; };
const idem = (v) => nonblank('idempotency_key', v, 128);
const rowVersion = (v) => { if (!Number.isInteger(v) || v < 1) throw new Error('expected_row_version must be a positive integer'); return v; };
const prio = (v) => { if (!Number.isInteger(v) || v < 1 || v > 4) throw new Error('priority must be 1..4'); return v; };
const labelList = (v) => { if (!Array.isArray(v) || v.length > 64 || v.some((l) => typeof l !== 'string' || !l.trim() || l.length > 128)) throw new Error('labels must be a short list of non-blank strings'); return v; };
const scope = (a) => ({ project_id: uuid('project_id', a?.projectId), issue_id: uuid('issue_id', a?.issueId) });
const versioned = (a) => ({ ...scope(a), expected_row_version: rowVersion(a?.write?.expected_row_version), idempotency_key: idem(a?.write?.idempotency_key) });
const STATUSES = ['Open', 'InProgress', 'Closed', 'Cancelled'];
const SLOTS = ['openrouter', 'anthropic', 'openai', 'bedrock', 'pioneer', 'inception', 'deepseek', 'groq', 'mistral'];
/** A credential value the isolated mode will forward: only the synthetic marker the end-to-end steps type. */
export const SYNTHETIC_SECRET = /^synthetic-e2e-[a-z0-9-]{4,64}$/;
const slotOf = (a) => { if (!SLOTS.includes(a?.slot)) throw new Error(`${JSON.stringify(a?.slot)} is not a provider key slot`); return a.slot; };

export const WRITE_COMMANDS = {
  create_issue: {
    method: 'CreateIssueV2',
    params: (a) => {
      const d = a?.draft;
      if (!d || typeof d !== 'object') throw new Error('draft must be an object');
      const out = { project_id: uuid('project_id', a?.projectId), title: nonblank('title', d.title, 1024), body: str('body', d.body ?? '', 200_000) };
      if (d.priority != null) out.priority = prio(d.priority);
      out.labels = labelList(d.labels ?? []);
      if (d.assignee != null) out.assignee = nonblank('assignee', d.assignee, 256);
      out.idempotency_key = idem(d.idempotency_key);
      return out;
    },
  },
  update_issue: {
    method: 'UpdateIssue',
    params: (a) => {
      const patch = a?.write?.patch;
      if (!patch || typeof patch !== 'object') throw new Error('patch is required');
      const p = {};
      for (const [k, v] of Object.entries(patch)) {
        if (k === 'title') p.title = nonblank(k, v, 1024);
        else if (k === 'body') p.body = str(k, v, 200_000);
        else if (k === 'labels') p.labels = labelList(v);
        else if (k === 'priority') p.priority = v === null ? null : prio(v);
        else if (k === 'assignee') p.assignee = v === null ? null : nonblank(k, v, 256);
        else throw new Error(`unknown patch field ${k}`);
      }
      if (!Object.keys(p).length) throw new Error('nothing to change');
      return { ...versioned(a), patch: p };
    },
  },
  update_issue_status: {
    method: 'UpdateIssueStatusV2',
    params: (a) => {
      if (!STATUSES.includes(a?.write?.status)) throw new Error('status is not one of the Issue statuses');
      return { ...versioned(a), status: a.write.status };
    },
  },
  toggle_pin: { method: 'TogglePin', params: (a) => ({ session_id: uuid('session_id', a?.sessionId) }) },
  archive_issue: { method: 'ArchiveIssue', params: (a) => versioned(a) },
  restore_issue: { method: 'RestoreIssue', params: (a) => versioned(a) },
  set_provider_credential: {
    methods: ['SetProviderCredential', 'RotateProviderCredential'],
    method: (a) => (a?.rotate ? 'RotateProviderCredential' : 'SetProviderCredential'),
    params: (a) => {
      if (typeof a?.secret !== 'string' || !SYNTHETIC_SECRET.test(a.secret)) throw new Error('isolated-write forwards synthetic credentials only (synthetic-e2e-…)');
      return { slot: slotOf(a), secret: a.secret };
    },
    wrap: redactCredential,
    allowSecretArg: true,
  },
  clear_provider_credential: { method: 'ClearProviderCredential', params: (a) => ({ slot: slotOf(a) }), wrap: redactCredential },
  // Local #125: mirrors src-tauri/src/settings_state.rs mode_params (the TUI slugs, never an interrupt).
  set_model_control_mode: {
    method: 'UpdateModelControlPolicy',
    params: (a) => {
      const modes = ['normal', 'pause-background', 'deny-paid', 'local-only', 'stop-all'];
      if (!modes.includes(a?.mode)) throw new Error(`unknown model control mode ${JSON.stringify(a?.mode)}`);
      return { mode: a.mode.replace(/-/g, '_'), interrupt_active: false };
    },
  },
  // #113: the file editor and the explorer's changes. They mirror src-tauri/src/files.rs (closed key sets, exactly
  // one root, no path outside the root judged here: rsid refuses those). Content is text; a version is a short token.
  write_project_file: {
    method: 'WriteProjectFile',
    params: fileRequest(['project_id', 'session_id', 'path', 'content', 'expected_version', 'force'], (o) => {
      if (typeof o.path !== 'string' || !o.path.trim()) throw new Error('path names no file');
      if (typeof o.content !== 'string') throw new Error('content must be text');
      if (o.expected_version !== undefined) str('expected_version', o.expected_version, 256);
      if (o.force !== undefined && typeof o.force !== 'boolean') throw new Error('force must be true or false');
      return o;
    }),
  },
  create_project_entry: {
    method: 'CreateProjectEntry',
    params: fileRequest(['project_id', 'session_id', 'path', 'kind'], (o) => {
      if (typeof o.path !== 'string' || !o.path.trim()) throw new Error('path names no file');
      if (o.kind !== 'file' && o.kind !== 'dir') throw new Error('kind must be "file" or "dir"');
      return o;
    }),
  },
  move_project_entry: { method: 'MoveProjectEntry', params: fileRequest(['project_id', 'session_id', 'from', 'to'], transferShape) },
  copy_project_entry: { method: 'CopyProjectEntry', params: fileRequest(['project_id', 'session_id', 'from', 'to'], transferShape) },
  trash_project_entry: {
    method: 'TrashProjectEntry',
    params: fileRequest(['project_id', 'session_id', 'path'], (o) => {
      if (typeof o.path !== 'string' || !o.path.trim()) throw new Error('path names no file');
      return o;
    }),
  },
  undo_project_file_operation: {
    method: 'UndoProjectFileOperation',
    params: fileRequest(['project_id', 'session_id'], (o) => { delete o.path; return o; }),
  },
  // Local #128: the graph review's `r` (graphs.rs execute_workflow_params, bind_session_workflow_params, upsert_params): save, run
  // and link the selected session. The run starts a real execution in the ISOLATED rsid only.
  graph_upsert_workflow: { method: 'UpsertWorkflowDefinition', params: (a) => upsertParams(a?.document) },
  graph_execute_workflow: {
    method: 'ExecuteWorkflow',
    params: (a) => {
      checkDefinition(a?.workflow);
      return {
        workflow_id: uuid('workflow_id', a?.workflowId), workflow: a.workflow, input: null, dry_run: false,
        project_id: a?.projectId ? uuid('project_id', a.projectId) : null, working_dir: null, parent_id: null,
      };
    },
  },
  graph_bind_session_workflow: {
    method: 'UpdateSessionWorkflow',
    params: (a) => ({ session_id: uuid('session_id', a?.sessionId), workflow_id: uuid('workflow_id', a?.workflowId) }),
    wrap: () => ({ ok: true }),
  },
  update_daemon_config: {
    method: 'UpdateDaemonConfig',
    params: (a) => {
      const ups = a?.updates;
      if (!Array.isArray(ups) || !ups.length || ups.length > 32) throw new Error('updates must be a short list');
      return {
        updates: ups.map((u) => {
          if (!/^[a-z0-9_.]{1,96}$/.test(u?.field ?? '') || SECRET_FIELD.test(u.field)) throw new Error(`${JSON.stringify(u?.field)} is not a plain daemon config field`);
          if (!(u.value === null || ['boolean', 'number', 'string'].includes(typeof u.value))) throw new Error(`${u.field} must be a plain value`);
          return { field: u.field, value: u.value };
        }),
      };
    },
  },
};

const MANUAL_SECTIONS = [
  { title: 'Getting started', html: '<h1>Getting started</h1><p>Press <code>n</code> to start a session, <code>Enter</code> to open one, <code>?</code> for help.</p><table><tr><th>Key</th><th>Does</th></tr><tr><td><code>j</code> / <code>k</code></td><td>Move down or up the session list</td></tr><tr><td><code>/</code></td><td>Filter sessions</td></tr></table>' },
  { title: 'Sessions', html: '<h1>Sessions</h1><p>A session is one conversation with a provider. Send a message, answer a question, or stop a turn from the session view.</p><ul><li>Archive keeps a session out of the way.</li><li>Restore brings it back.</li></ul>' },
  { title: 'Settings', html: '<h1>Settings</h1><p>Open Actions and choose Settings to change a setting. Changes apply to the running rsid.</p>' },
];

/** Commands answered by the shim itself with no daemon traffic (no backend state to read). */
export const LOCAL_COMMANDS = {
  live_link: () => false,
  graph_validate: (a) => validateWorkflowLocal(a?.workflow),
  set_app_menu: () => null,
  sync_menu_state: () => null,
  set_attention_count: () => null,
  // notify_session posts a macOS notification (src-tauri/src/notify.rs, local #294); a headless page posts none, and the smoke must never reach the desktop.
  notify_session: () => null,
  // notify_permission reads macOS's notification permission and open_notification_settings opens System Settings (local #300); a headless page has neither, so it reports unknown and opens nothing.
  notify_permission: () => 'unknown',
  open_notification_settings: () => null,
  // set_window_theme only themes the native title bar (src-tauri/src/window_theme.rs, local #283); a headless page has none.
  set_window_theme: () => null,
  // boot_report only prints the boot line to the process log (src-tauri/src/boot.rs); the page keeps it in data-boot-report.
  boot_report: () => null,
  // render_markdown is a pure Rust function (markdown.rs); the shim renders a small subset instead (paragraphs, bullet and numbered
  // lists, fenced code, `code` and **bold**), escaping first so the output is innerHTML-safe (local #313).
  render_markdown: (a) => (Array.isArray(a?.texts) ? a.texts : []).map((t) => renderMarkdownSubset(String(t))),
  // The embedded terminal's PTY is Tauri-only. These stubs open no shell and run nothing: they let the overlay render.
  pty_spawn: () => ({ id: 'smoke-pty', cwd: '/' }),
  pty_write: () => null,
  pty_resize: () => null,
  pty_kill: () => true,
  // The native folder chooser (dialog.rs) cannot open in a headless page; the stub answers like a person who picked a folder (local #271).
  // manual_sections renders the embedded manual (commandmode.rs); the shim answers a short manual of the same shape ({ title, html }).
  manual_sections: () => MANUAL_SECTIONS,
  // The manager policy planners are pure Rust (manager_ops.rs); the shim answers the common case: a plain execute policy, no suggestions.
  choose_folder: () => '/tmp/rsi-smoke-picked-folder',
  providers: () => ['Claude', 'Codex', 'Pioneer', 'OpenRouter', 'Bedrock', 'Local', 'Antigravity', 'CodexAppServer', 'Harness'],
  // Start rsid (src-tauri/src/service.rs, local #232) asks launchd or systemd to start the installed rsid.
  // The smoke starts nothing: it reports no installed service unless a first-run scenario overrides it.
  daemon_service: () => ({ kind: 'not-installed', manager: 'launchd', looked: ['~/Library/LaunchAgents/dev.rsi.rsid.plist'] }),
  start_daemon: () => { throw new Error('no installed rsid service was found (the smoke starts nothing)'); },
  restart_daemon: () => { throw new Error('no installed rsid service was found (the smoke restarts nothing)'); },
};

/**
 * The gate in front of POST /__invoke (hub constraint): the per-run secret that only the injected
 * shim knows, and the harness server's own Origin. `headers` are Node's lower-cased request headers.
 */
export function authorizeInvoke({ method, headers }, { token, origin }) {
  if (method !== 'POST') return { ok: false, reason: 'method' };
  if (headers['x-smoke-token'] !== token) return { ok: false, reason: 'token' };
  if (headers.origin !== origin) return { ok: false, reason: 'origin' };
  return { ok: true };
}

/** Defence in depth: the RPC method must be here even if READ_COMMANDS is edited wrongly. */
export const READ_METHODS = new Set(Object.values(READ_COMMANDS).map((c) => c.method));
/** The exact methods the isolated-write mode may add. */
export const WRITE_METHODS = new Set(Object.values(WRITE_COMMANDS).flatMap((c) => c.methods ?? [c.method]));

const SECRETISH = /secret|token|password|api[_-]?key|credential/i;
function carriesSecret(v, depth = 0) {
  if (depth > 6 || v === null || typeof v !== 'object') return false;
  return Object.entries(v).some(([k, x]) => SECRETISH.test(k) || carriesSecret(x, depth + 1));
}

/**
 * `call(method, params)` performs the daemon request. `record(entry)` receives every decision:
 * { cmd, method?, outcome: 'forwarded' | 'local' | 'refused' | 'error', reason? }.
 */
export function createBridge({ call, record = () => {}, isolated = null, local = {} }) {
  // Write mode needs the socket of an isolated rsid this run started; anything else throws here, at construction.
  if (isolated !== null) assertIsolatedSocket(isolated.socket);
  return {
    async invoke(cmd, args) {
      if (Object.hasOwn(LOCAL_COMMANDS, cmd)) {
        record({ cmd, outcome: 'local' });
        // `local` lets a first-run scenario answer a shim-local command its own way (only names already in LOCAL_COMMANDS).
        return (Object.hasOwn(local, cmd) ? local[cmd] : LOCAL_COMMANDS[cmd])(args);
      }
      const writable = isolated !== null && Object.hasOwn(WRITE_COMMANDS, cmd);
      const entry = writable ? WRITE_COMMANDS[cmd] : Object.hasOwn(READ_COMMANDS, cmd) ? READ_COMMANDS[cmd] : null;
      const method = entry ? (typeof entry.method === 'function' ? entry.method(args) : entry.method) : null;
      const known = writable ? WRITE_METHODS.has(method) : READ_METHODS.has(method);
      if (!entry || !known) {
        record({ cmd, outcome: 'refused', reason: 'not on the read-only allowlist' });
        throw new Error(`smoke bridge: '${cmd}' is not on the read-only allowlist`);
      }
      if (!entry.allowSecretArg && carriesSecret(args)) {
        record({ cmd, method, outcome: 'refused', reason: 'arguments carry a credential' });
        throw new Error(`smoke bridge: '${cmd}' arguments carry a credential`);
      }
      let params;
      try { params = entry.params(args); } catch (e) {
        record({ cmd, method, outcome: 'refused', reason: String(e.message) });
        throw e;
      }
      try {
        const result = await call(method, params);
        record({ cmd, method, outcome: 'forwarded', ...(writable ? { write: true } : {}), ...(writable && typeof result?.execution_id === 'string' ? { execution_id: result.execution_id } : {}) });
        return entry.wrap ? entry.wrap(result) : result;
      } catch (e) {
        record({ cmd, method, outcome: 'error', reason: String(e.message) });
        if (entry.fallback) return entry.fallback(e);
        throw e;
      }
    },
  };
}

/**
 * A minimal Markdown subset for the shim: blank-line paragraphs, `-`/`*`/`1.` lists, fenced code, inline `code` and **bold**. Every
 * piece of text is escaped before any tag is added. Not a Markdown engine: markdown.rs is the real one; this only keeps a screenshot from
 * showing raw `-`, `**` and backticks that a reader could mistake for a rendering bug.
 */
export function renderMarkdownSubset(text) {
  const inline = (t) => escapeHtml(t).replace(/`([^`]+)`/g, '<code>$1</code>').replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  const out = [];
  const lines = text.replace(/\r\n?/g, '\n').split('\n');
  let para = [];
  let list = null;
  const flush = () => {
    if (para.length) out.push(`<p>${para.map(inline).join('<br>')}</p>`);
    if (list) out.push(`<${list.tag}>${list.items.map((i) => `<li>${inline(i)}</li>`).join('')}</${list.tag}>`);
    para = [];
    list = null;
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (/^```/.test(line)) {
      flush();
      const code = [];
      for (i++; i < lines.length && !/^```/.test(lines[i]); i++) code.push(lines[i]);
      out.push(`<pre><code>${escapeHtml(code.join('\n'))}</code></pre>`);
      continue;
    }
    const item = /^\s*(?:([-*])|(\d+)[.)])\s+(.*)$/.exec(line);
    if (item) {
      const tag = item[1] ? 'ul' : 'ol';
      if (list && list.tag !== tag) flush();
      if (para.length) flush();
      list ??= { tag, items: [] };
      list.items.push(item[3]);
    } else if (!line.trim()) flush();
    else {
      if (list) flush();
      para.push(line);
    }
  }
  flush();
  return out.join('');
}

/** How many times the shim sends one invoke before it reports the transport failure (local #621). */
export const SHIM_ATTEMPTS = 4;

/**
 * The script injected before the app: the only Tauri surface the frontend uses.
 *
 * Under CPU contention headless Chrome cancels requests of a live document before they reach the network stack ("TypeError:
 * Failed to fetch", ERR_ABORTED with `canceled`; the net-log shows no failed request, local #621/#623). Only the page's
 * transport fails; the bridge never saw the call. Each invoke therefore carries a call id and is re-sent on such a rejection:
 * the server runs a call id at most once and replays its answer, so a retried write cannot run twice. A rejection that
 * persists through every attempt is thrown, and each retry is kept in `__smokeInvokeRetries` for the failure report.
 */
export function shimSource(token) {
  return `(() => {
  const TOKEN = ${JSON.stringify(token)};
  const ATTEMPTS = ${SHIM_ATTEMPTS};
  const RUN = Math.random().toString(36).slice(2, 10);
  let calls = 0;
  const invoke = async (cmd, args) => {
    const call = RUN + '-' + (++calls);
    for (let attempt = 1; ; attempt++) {
      let j;
      try {
        const r = await fetch('/__invoke', { method: 'POST', headers: { 'content-type': 'application/json', 'x-smoke-token': TOKEN, 'x-smoke-call': call }, body: JSON.stringify({ cmd, args: args ?? null }) });
        j = await r.json();
      } catch (e) {
        if (attempt >= ATTEMPTS) throw e;
        (globalThis.__smokeInvokeRetries ??= []).push({ cmd, attempt, error: String(e), at: Math.round(performance.now()) });
        await new Promise((resolve) => setTimeout(resolve, 50 * attempt));
        continue;
      }
      if (j.error) throw j.error;
      return j.result;
    }
  };
  globalThis.__TAURI__ = { core: { invoke }, event: { listen: async () => () => {} } };
})();`;
}
