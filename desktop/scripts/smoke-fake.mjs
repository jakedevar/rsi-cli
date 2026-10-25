// A fake rsid for the desktop smoke: a Unix-socket JSON-RPC server (one request line in, one
// response line out, as src-tauri/src/daemon.rs speaks it) with canned read results. It records every
// method it receives, so a test can assert that nothing but reads ever reached a daemon.
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const SATELLITE_PEER_ID = 'abababab-abab-4aba-8aba-abababababab';
export const SETTLEMENT_RUN_ID = 'ffffffff-ffff-4fff-8fff-ffffffffffff';
export const PROJECT_ID = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';
/** What every rsid lists since hub #3315: the built-in RSI home project (fixed id, the daemon's $HOME). Desktop #319. */
export const HOME_PROJECT = { id: '00000000-0000-4000-8000-727369686f6d', name: 'RSI home', path: '/Users/operator', color: '#89b4fa' };
export const SESSION_IDS = {
  lead: '11111111-1111-4111-8111-111111111111',
  worker: '22222222-2222-4222-8222-222222222222',
  done: '33333333-3333-4333-8333-333333333333',
  // A WaitingApproval session with a pending question (the approval banner), and one in the Trash.
  waiting: '55555555-5555-4555-8555-555555555555',
  deleted: '66666666-6666-4666-8666-666666666666',
  // A root session that has finished (Completed): the list keeps it, muted (local #264).
  completed: 'dddddddd-dddd-4ddd-8ddd-dddddddddddd',
  // An archived session whose prompt the filter finds (local #272).
  archived: 'abababab-abab-4aba-8aba-abababababab',
};
// Seats that hold manager authority. They are not listed sessions: the manager views read them by id.
export const MANAGER_IDS = {
  globalSeat: '44444444-4444-4444-8444-444444444444',
  pmSeat: '99999999-9999-4999-8999-999999999999',
  globalNode: '88888888-8888-4888-8888-888888888888',
  grant: '77777777-7777-4777-8777-777777777777',
  invocation: 'cccccccc-cccc-4ccc-8ccc-cccccccccccc',
};
const T = '2026-01-02T03:04:05.000000000Z';
const here = dirname(fileURLToPath(import.meta.url));

const session = (id, title, status, extra = {}) => ({
  id, status, provider: 'Claude', session_kind: 'Agent', title, query: `${title} (query)`, working_dir: '/tmp/fake-project',
  model: 'default', project_id: PROJECT_ID, created_at: T, updated_at: T, ...extra,
});

const issue = (n, title, status, priority) => ({
  issue: {
    id: `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`, project_id: PROJECT_ID, display_number: n, title, body: `Body of ${title}`,
    status, priority, labels: ['desktop'], created_at: T, updated_at: T, row_version: 1,
  },
  readiness: status === 'Open' ? 'ready' : 'not_eligible', open_blocker_count: 0, dependent_count: 0,
});

/** `PendingQuestion` (camelCase `multiSelect`). */
const PENDING_QUESTION = {
  questions: [{
    question: 'Which checks should run before the fix lands?', header: 'Checks', multiSelect: false,
    options: [{ label: 'tests', description: 'Run the desktop test suite' }, { label: 'smoke', description: 'Run the fake-daemon smoke' }],
  }],
};

const conversation = (id) => [
  { id: 1, session_id: id, sequence: 1, event_type: 'Message', role: 'User', created_at: T, content: 'Fix the list view.' },
  { id: 2, session_id: id, sequence: 2, event_type: 'Message', role: 'Assistant', created_at: T, content: 'I fixed the list view and added a test.' },
];

// ---------- manager surfaces ----------
// Every shape below is the daemon's own response type (crates/rsi-common: manager_tree.rs, global_manager.rs,
// manager_budget.rs, harness_manager.rs, harness_manager_v2.rs, rpc.rs, model_control.rs, types.rs), field for field.
// The section rows of GetHarnessManagerState are free-form JSON on the wire; they follow the ledger's row builders
// (rsid-store manager_ledger/inspect.rs: `type`, `key`, `row_version`, then the record payload).

const LAUNCH = { provider: 'Claude', model: 'opus', effort: 'high' };
const CAPABILITIES = ['work_plan', 'lead_control', 'session_create', 'integration'];
/** `ManagerPolicyV2`, every field. */
const MANAGER_POLICY = {
  mode: 'execute', paused: false, paused_epic_ids: [], capabilities: CAPABILITIES, group_ids: [], allow_create_groups: false,
  max_created_containers: 8, max_created_sessions: 64, max_active_sessions: 8, provider_limits: [], allowed_launches: [LAUNCH],
  max_recovery_attempts: 2, retry_delay_seconds: 60, request_timeout_seconds: 900, max_spend_usd: 200,
  minimum_tier2_reviewer_tier: 'standard', daemon_setting_bounds: [],
};
const GLOBAL_KEY = `portfolio:${MANAGER_IDS.globalNode}`;
const PROJECT_KEY = `project:${PROJECT_ID}`;
const DECISION_KEY = `question:${SESSION_IDS.worker}`;

/** `GlobalManagerGrantV1`. */
const globalGrant = () => ({
  grant_id: MANAGER_IDS.grant, grant_version: 3, seat_session_id: MANAGER_IDS.globalSeat, state: 'active', project_ids: [PROJECT_ID],
  allowed_launches: [LAUNCH], project_policy: MANAGER_POLICY, operator_origin: 'operator', created_at: T, updated_at: T,
});

/** `GlobalPmSeatV1`. */
const pmSeat = () => ({
  session_id: MANAGER_IDS.pmSeat, status: 'Running', provider: 'Claude', model: 'opus', effort: 'high', context_fill_pct: 37.5,
  cost_usd: 4.25, updated_at: T, scope_version: 2, pending_question: false,
});

/** `ManagerTreeRowV1`. */
const treeRows = () => [
  {
    key: GLOBAL_KEY, parent_key: null, depth: 0, kind: 'portfolio', label: 'Global manager', tier_label: 'global', grantor: 'operator',
    project_id: null, node_id: MANAGER_IDS.globalNode, epic_id: null, scope: '1 project',
    seat: { session_id: MANAGER_IDS.globalSeat, status: 'Running', model: 'opus', context_fill_pct: 42.5, updated_at: T },
    focus_session_id: MANAGER_IDS.globalSeat,
    grant: { grant_version: 3, capabilities: CAPABILITIES, max_active_sessions: 12, max_created_sessions: 128, max_created_containers: 16, max_direct_reports: 4, max_spend_usd: 500, reserved: [{ resource_kind: 'created_sessions', amount: 64 }] },
    launches: [LAUNCH], load: { running_workers: 2, direct_reports: 1, pending_escalations: 0, pending_decisions: 1 }, complete: true,
  },
  {
    key: PROJECT_KEY, parent_key: GLOBAL_KEY, depth: 1, kind: 'project', label: 'Fake project', grantor: `node:${MANAGER_IDS.globalNode}`,
    project_id: PROJECT_ID, node_id: null, epic_id: null, scope: 'project',
    seat: { session_id: MANAGER_IDS.pmSeat, status: 'Running', model: 'opus', context_fill_pct: 37.5, updated_at: T },
    focus_session_id: MANAGER_IDS.pmSeat, grant: null, launches: [LAUNCH],
    load: { running_workers: 2, direct_reports: 1, pending_escalations: 0, pending_decisions: 1 }, complete: true,
  },
  {
    key: `epic:${SESSION_IDS.lead}`, parent_key: PROJECT_KEY, depth: 2, kind: 'epic', label: 'Fake lead', project_id: PROJECT_ID, node_id: null,
    epic_id: SESSION_IDS.lead, scope: null, seat: null, focus_session_id: SESSION_IDS.lead, grant: null,
    load: { running_workers: 2, direct_reports: null, pending_escalations: 0, pending_decisions: 0 }, complete: true,
  },
];

/** One `GetHarnessManagerState` page. `policy` is `HarnessManagerPolicyConfigV2`. */
const statePage = (section, rows) => ({ observed_at: T, scope_version: 2, policy: managerPolicy(), section, rows, next_cursor: null, complete: true });
const managerPolicy = () => ({ project_id: PROJECT_ID, manager_session_id: MANAGER_IDS.pmSeat, scope_version: 2, row_version: 4, policy: MANAGER_POLICY, updated_at: T, revoked: false });

const kpi = (kind, over) => ({
  type: 'overview', key: kind, kind, scope_revision: 1, denominator: 5, weight_denominator: 5, accepted: 3, accepted_weight: 3, integrated: 2,
  integrated_weight: 2, unknown: 0, partial: 1, ready: 1, missing_work_scope: false, unplanned_epics: [], manager_available: true, ...over,
});
const STATE_ROWS = {
  overview: () => [kpi('program', {}), kpi('product', { denominator: 4, weight_denominator: 4, accepted: 1, accepted_weight: 1, integrated: 0, integrated_weight: 0, partial: 2, ready: 1 })],
  // A pending decision: `record_row` (payload fields, then type/key/row_version/created_at/updated_at) with the structured options.
  decisions: () => [{
    type: 'decision', key: DECISION_KEY, session_id: SESSION_IDS.worker, status: 'pending', question: 'Land the list-view fix now, or wait for the terminal overlay change?',
    options: [{ label: 'A', detail: 'Land now', recommended: true }, { label: 'B', detail: 'Wait for the overlay change', recommended: false }],
    target_digest: `sha256:${'ab'.repeat(32)}`, row_version: 3, created_at: T, updated_at: T,
  }],
  requests: () => [{
    type: 'request', key: 'req-1', request_id: 'req-1', epic_id: SESSION_IDS.lead, recipient_session_id: SESSION_IDS.lead, message: 'Report the list-view test status.',
    state: 'pending', row_version: 1, replied: false, accepted: false, overdue: false, unanswered: true,
  }],
  health: () => [{
    type: 'health', key: SESSION_IDS.lead, epic_id: SESSION_IDS.lead, title: 'Fake lead', epic_status: 'Running', lead_state: 'current',
    stuck: [], complete: true, truncated: [],
  }],
};

/** `BudgetCapV1`. */
const cap = (field, used, limit, over = {}) => ({ field, used, limit, binding: false, spend_unknown: false, ...over });
/** `GlobalManagerBudgetV1`: one covered project, a portfolio node above the project manager. */
const budgetSnapshot = () => ({
  node_id: MANAGER_IDS.globalNode, node_label: 'Global manager',
  projects: [{
    project_id: PROJECT_ID, project_name: 'Fake project',
    chain: [
      { kind: 'portfolio', node_id: MANAGER_IDS.globalNode, label: 'Global manager', version: 3, epoch: 1, counted_since: T,
        caps: [cap('created_sessions', 12, 128), cap('active_sessions', 2, 12), cap('created_containers', 1, 16), cap('spend_usd', 12.5, 500)] },
      { kind: 'project', node_id: PROJECT_ID, label: 'Fake project manager', version: 4, epoch: 2, counted_since: T,
        caps: [cap('created_sessions', 12, 64), cap('active_sessions', 2, 8, { binding: true }), cap('created_containers', 1, 8), cap('spend_usd', 12.5, 200)] },
    ],
  }],
  resets: 'no timed reset: a portfolio node counts created sessions and containers since its current grant began; spend is NOT reset by an edit; a PM counts over its current scope; active sessions are live',
});

/** `GlobalManagerWorkspaceV1`. */
const workspace = () => ({
  grant: globalGrant(),
  seat: {
    session_id: MANAGER_IDS.globalSeat, project_id: PROJECT_ID, status: 'Running', provider: 'Claude', model: 'opus', context_fill_pct: 42.5,
    cost_usd: 6.5, updated_at: T, pending_question: false,
  },
  projects: [{
    overview: {
      project_id: PROJECT_ID, name: 'Fake project', path: '/tmp/fake-project', manager: pmSeat(),
      policy: { policy_version: 4, mode: 'execute', revoked: false, paused: false, capabilities: CAPABILITIES },
      issues: { open: 1, in_progress: 1, open_operator_requests: 0 }, running_sessions: 2, waiting_approval_sessions: 1, pending_questions: 1,
      pending_approvals: 0, standing_directives: [],
    },
    scope_revoked: false,
  }],
  missing_project_ids: [],
});

/** `ModelBudgetPolicy`. */
const budgetPolicy = () => ({
  scope_kind: 'global', scope_id: null, purpose: null, model_tier: null, effort: null, ceiling_model_tier: null, ceiling_effort: null, max_calls: null,
  max_total_tokens: 5_000_000, max_input_tokens: null, max_output_tokens: null, max_cache_creation_tokens: null, max_cache_read_tokens: null,
  max_reasoning_tokens: null, max_embedding_inputs: null, max_wall_time_ms: null, max_concurrency: 4, max_retries: null, max_calls_per_window: 600,
  rate_window_seconds: 3600, alert_threshold_ratio: 0.8,
});
/** `ModelControlStatusReport`. */
const modelControlStatus = () => ({
  mode: 'normal', mode_updated_at: T, restart_required_fields: [], circuit_state: 'closed', circuit_reason: '', circuits: [], policies: [budgetPolicy()],
  active_invocations: [], recent_invocations: [], recent_denials: [], recent_budget_alerts: [],
});
/** `UsageStats`. */
const usageStats = () => ({
  lifetime_chats: 42, total_cost_usd: 123.45, total_input_tokens: 9_876_543, total_output_tokens: 1_234_567, total_cache_creation_tokens: 200_000,
  total_cache_read_tokens: 4_000_000, total_work_time_ms: 36_000_000,
  per_model: [{ model: 'opus', chats: 30, cost_usd: 100, input_tokens: 8_000_000, output_tokens: 1_000_000, cache_creation_tokens: 150_000, cache_read_tokens: 3_000_000, work_time_ms: 30_000_000 }],
  timeline: [{ day: '2026-01-02', cost_usd: 10, input_tokens: 1_000_000, output_tokens: 100_000, cache_creation_tokens: 10_000, cache_read_tokens: 500_000 }],
});
/** `SessionModelSwitchOptions`. */
const switchOptions = (sessionId) => ({
  session_id: sessionId, provider: 'Claude', model: 'opus', effort: 'high', model_invocation_id: MANAGER_IDS.invocation, switchable: true,
  unavailable_reason: null, keeps_context: true, context_note: 'The switch keeps the conversation and applies on the next turn.',
  model_allowlist: ['opus', 'sonnet', 'haiku'], pending: null,
});
/** The dashboard fixture (`GlobalDashboard`, tests/fixtures/global-dashboard.json) answers the window the page asked for. */
const DASHBOARD_WINDOWS = { '1h': 3600, '24h': 86400, '7d': 604800 };
const dashboard = (p) => {
  const d = JSON.parse(readFileSync(join(here, '..', 'tests', 'fixtures', 'global-dashboard.json'), 'utf8'));
  const window = DASHBOARD_WINDOWS[p.window] ? p.window : '24h';
  // `as_of` is now: the page prints its age.
  return { ...d, as_of: new Date().toISOString().replace('Z', '000000Z'), window, window_seconds: DASHBOARD_WINDOWS[window] };
};

/** One FleetUsage window scaled by `n` invocations. */
const FLEET_USAGE = (n) => ({ invocations: n, errors: n > 100 ? 2 : 0, input: n * 12_000, output: n * 2_000, cache_read: n * 30_000, cache_write: n * 3_000, cost: n * 0.12, unknown_usage: 0 });
/** The canned read results. Keys are rsid RPC method names. */
export const FIXTURES = {
  GetHealthStatus: () => ({ status: 'ok', version: 'fake' }),
  // The running build and the newest local one (DaemonInfoV1): `differs: false`, so the newer-build notice (local #322) stays off.
  GetDaemonInfo: () => ({ build_sha: 'a'.repeat(40), local_build: { sha: 'a'.repeat(40), path: '/fake/target/release/rsid', built_at: '2026-10-20T08:00:00.000000000Z', differs: false } }),
  ListProjects: () => [{ id: PROJECT_ID, name: 'Fake project', path: '/tmp/fake-project', color: '#7aa2f7' }, HOME_PROJECT],
  ListSessions: () => [
    session(SESSION_IDS.lead, 'Fake lead', 'Running', { session_kind: 'Epic' }),
    session(SESSION_IDS.worker, 'Fake worker', 'Running', { parent_id: SESSION_IDS.lead }),
    session(SESSION_IDS.done, 'Fake finished worker', 'Failed', { parent_id: SESSION_IDS.lead }),
    session(SESSION_IDS.waiting, 'Fake waiting worker', 'WaitingApproval', { parent_id: SESSION_IDS.lead, pending_question: PENDING_QUESTION }),
    session(SESSION_IDS.completed, 'Fake completed session', 'Completed', { query: 'export invoices to csv' }),
  ],
  // The Trash: one deleted session.
  ListDeletedSessions: () => [session(SESSION_IDS.deleted, 'Fake deleted worker', 'Deleted', { updated_at: '2026-01-01T03:04:05.000000000Z' })],
  ListArchivedSessions: () => [session(SESSION_IDS.archived, 'Fake archived session', 'Archived', { query: 'export old invoices to csv' })],
  ListLabels: () => [],
  // Reads of the views that open from Actions (local #298): realistic, so each view can be audited, not an empty list or null.
  ListMcpServers: () => ({ servers: [
    { definition: { id: 'github', command: 'npx', args: ['-y', '@modelcontextprotocol/server-github'], secret_env_names: ['GITHUB_TOKEN'], working_dir: null, enabled: true },
      credential: { id: 'github', state: 'set', fingerprint: 'a1b2c3d4', generation: 2 } },
    { definition: { id: 'filesystem', command: 'mcp-fs', args: ['/tmp/fake-project'], secret_env_names: [], working_dir: '/tmp/fake-project', enabled: false },
      credential: { id: 'filesystem', state: 'unset' } },
  ] }),
  ListSatellitePeers: () => ({ revision: 3, peers: [{
    config: { peer_id: SATELLITE_PEER_ID, label: 'mac-mini', expected_installation_id: 'inst-mac-mini', enabled: true, read_enabled: true, dispatch_enabled: false },
    row_version: 2,
    links: [{ link_id: '13131313-1313-4313-8313-131313131313', direction: 'hub_to_satellite', socket_path: '/Users/fake/.rsi/daemon.sock', ssh_target: 'mac-mini.local', trust_reference: 'ssh-ed25519 SHA256:fake', enabled: true, priority: 1 }],
    observation: { state: 'connected', installation_id: 'inst-mac-mini', incarnation_id: 'inc-1', last_observed_at: '2026-10-20T12:00:00.000000000Z', last_error: null, cached_session_count: 1, stale: false, sessions_running: 1, workers_parked: 2, open_jobs: 2 },
    dispatch_scope: [], dispatch_project_scope: [],
  }, {
    // An older satellite that reports no activity: the registry shows `unknown` for each count (#323).
    config: { peer_id: '14141414-1414-4414-8414-141414141414', label: 'laptop', expected_installation_id: null, enabled: true, read_enabled: true, dispatch_enabled: false },
    row_version: 1,
    links: [],
    observation: { state: 'paired', installation_id: null, incarnation_id: null, last_observed_at: null, last_error: null, cached_session_count: 0, stale: false },
    dispatch_scope: [], dispatch_project_scope: [],
  }] }),
  GetSatelliteScopeStatus: () => ({ inbound: [{ kind: 'project', id: PROJECT_ID, state: 'allowed' }] }),
  GetSatelliteInboundPolicy: () => ({ allowed_hub_installations: ['inst-hub'], scope_roots: ['/tmp/fake-project'], scope_projects: [PROJECT_ID] }),
  ListHubSatelliteSessions: (p) => ({ peer_id: p.peer_id, offset: p.offset ?? 0, next_offset: null, sessions: [{
    key: { peer_id: p.peer_id, remote_session_id: '12121212-1212-4212-8212-121212121212' },
    summary: { session_id: '12121212-1212-4212-8212-121212121212', title: 'Fake satellite worker', provider: 'Claude', status: 'Running', working_dir: '/tmp/fake-project', updated_at: '2026-10-20T12:00:00.000000000Z' },
  }] }),
  ListLegacyScratch: () => ({ candidates: [
    { path: '/tmp/fake-scratch/clean', kind: 'git_worktree', blocker: null, bytes: 2_097_152 },
    { path: '/tmp/fake-scratch/dirty', kind: 'git_worktree', blocker: 'dirty_worktree', bytes: 10_485_760, detail: '3 files changed' },
  ], refused_roots: 0, budget_exhausted: false }),
  ListSourceWorktreeCohorts: () => [{ repository_identity: 'git:fake-repo', canonical_repo_dir: '/tmp/fake-project', live_roots: 2, terminal_roots: 5 }],
  ListSourceWorktreeSettlementRuns: () => [{ run_id: SETTLEMENT_RUN_ID, repository_identity: 'git:fake-repo', state: 'completed',
    counts: { observed: 7, eligible: 5, retained: 2, settled: 5, refused: 0, recovery_required: 0, unattempted: 0 }, created_at: '2026-10-19T09:00:00.000000000Z' }],
  GetSourceWorktreeSettlementRun: (p) => ({ run_id: p.run_id, repository_identity: 'git:fake-repo', canonical_repo_dir: '/tmp/fake-project', unproven_survivors: [] }),
  GetDrainRestartStatus: () => ({ state: 'idle', active_sessions: [], requested_at: null }),
  ListSkills: () => ({ skills_dir: '/Users/fake/.claude/skills', truncated: false, skipped_outside_root: 0, skills: [
    { name: 'code-review', enabled: true, description: 'Review a diff for correctness bugs', has_skill_md: true },
    { name: 'notes-helper', enabled: false, description: null, has_skill_md: true },
  ] }),
  GetSkill: (p) => ({ name: p.name, enabled: true, path: `/Users/fake/.claude/skills/${p.name}/SKILL.md`, bytes: 96, truncated: false,
    content: `---\nname: ${p.name}\ndescription: A fake skill\n---\n\n# ${p.name}\n\nDo the fake thing, then stop.\n` }),
  ListPromptFiles: () => ({ dir: '/Users/fake/.rsi/prompts', truncated: false, files: [
    { name: 'prompt-20261019-101500.md', first_line: '# Review the audit findings', size: 412, mtime: '2026-10-19T10:15:00Z' },
    { name: 'release-notes.md', first_line: '# Draft the release notes', size: 1280, mtime: '2026-10-18T08:00:00Z' },
  ] }),
  GetPromptFile: (p) => ({ name: p.name, content: '# Review the audit findings\n\nList each finding with its evidence.\n', size: 66, mtime: '2026-10-19T10:15:00Z' }),
  // The manager views (fleet, escalations, friction, portfolio nodes, area nodes, restart) and the model picker.
  GetFleetOverview: () => ({
    as_of: T, agents_truncated: false, usage_truncated: false,
    agents: [
      { session: { id: SESSION_IDS.worker, provider: 'Claude', model: 'opus', effort: 'high', status: 'Running', title: 'Fake worker', context_fill_pct: 42 }, project: 'Fake project', role: 'implementer', turn_started_at: T },
      { session: { id: SESSION_IDS.waiting, provider: 'Codex', model: 'gpt-5.5', effort: 'medium', status: 'WaitingApproval', title: 'Fake waiting worker', context_fill_pct: 18 }, project: 'Fake project', role: 'reviewer', turn_started_at: null },
    ],
    groups: [
      { dimension: 'project', key: PROJECT_ID, label: 'Fake project', active: 2, windows: [FLEET_USAGE(4), FLEET_USAGE(30), FLEET_USAGE(210)] },
      { dimension: 'provider', key: 'Claude', label: 'Claude', active: 1, windows: [FLEET_USAGE(3), FLEET_USAGE(22), FLEET_USAGE(150)] },
      { dimension: 'provider', key: 'Codex', label: 'Codex', active: 1, windows: [FLEET_USAGE(1), FLEET_USAGE(8), FLEET_USAGE(60)] },
      { dimension: 'model', key: 'opus', label: 'opus', active: 1, windows: [FLEET_USAGE(3), FLEET_USAGE(22), FLEET_USAGE(150)] },
    ],
    totals: [FLEET_USAGE(4), FLEET_USAGE(30), FLEET_USAGE(210)],
  }),
  ListPortfolioNodes: () => ({ nodes: [{
    node_id: '14141414-1414-4414-8414-141414141414', tier_label: 'global', state: 'active', authority_epoch: 3, parent_node_id: null, max_direct_reports: 8, child_policy: null,
    grant: { grant_version: 3, seat_session_id: MANAGER_IDS.globalSeat, project_ids: [PROJECT_ID], allowed_launches: [], project_policy: { policy_version: 4, mode: 'execute', revoked: false, paused: false, capabilities: CAPABILITIES } },
  }] }),
  ListManagerNodes: () => ({ rows: [{
    node_id: '15151515-1515-4515-8515-151515151515', seat_root_session_id: SESSION_IDS.lead, state: 'active', grant_state: 'active', selector: { mode: 'project' },
    grant_version: 2, authority_epoch: 1, direct_reports: 1,
  }], next_after_node_id: null }),
  ListFrictionRollup: () => ({ window_hours: 24, rows: [
    { kind: 'tool_error', signature: 'scoped_test timed out waiting for a build slot', occurrences: 3, last_seen_at: T },
    { kind: 'refusal', signature: 'agent_issue_authority_denied on a related Issue', occurrences: 2, last_seen_at: T },
  ] }),
  ListOperatorEscalations: () => ({
    escalations: [{ hop_id: '16161616-1616-4616-8616-161616161616', state: 'open', hop: 1, source_ref: 'session:fake-lead', project_id: PROJECT_ID, reason: 'Needs an operator ruling on a release.', ruling: null }],
    notices: [{ message_id: '17171717-1717-4717-8717-171717171717', state: 'queued', source_ref: 'session:fake-lead', body: 'Weekly report is ready.' }],
    undelivered: [], next_undelivered_after: null,
  }),
  GetOperatorRestart: () => ({ pending: false, state: 'idle', forced: false, blockers: [], turns_in_flight: 0, release_by: null }),
  ListProviderCredentials: () => ({ env_compat: true, check_ttl_secs: 3600, credentials: [
    { slot: 'anthropic', state: 'set', fingerprint: 'sk-…a1b2', generation: 2, route: 'daemon', cli_exposure: 'none', check: { at: '2026-10-20T08:00:00Z', class: 'ok', http_status: 200 } },
    { slot: 'openrouter', state: 'unset', route: 'daemon', cli_exposure: 'none' },
  ] }),
  GetProviderLoginState: () => ({ providers: [
    { provider: 'Claude', login: { state: 'logged_in', method: 'oauth', checked_at: '2026-10-20T08:00:00Z' } },
    { provider: 'Codex', login: { state: 'logged_out', method: 'chatgpt' }, remedy: 'Run codex login in a terminal.' },
  ] }),
  GetProviderContractState: () => ({ providers: [
    { provider: 'Claude', contract: { contract_state: 'ok', cli_version: '2.0.0', checked_at: '2026-10-20T08:00:00Z' } },
    { provider: 'Codex', contract: { contract_state: 'ok', cli_version: '0.9.0', checked_at: '2026-10-20T08:00:00Z' } },
  ], codex_refuse_untrusted_custody_hook: false }),
  GetColdArchiveStatus: () => ({
    settings: { enabled: true }, live_db: { file_bytes: 734_003_200 }, totals: { staged_sessions: 12, verified_sessions: 12, cold_sessions: 40, restored_sessions: 0 },
    archives: [{ name: 'cold-2026-09.sqlite.zst', bytes: 188_743_680 }], compaction: { pending: false, due: false }, errors: [],
    last_run: { mode: 'stage', trigger: 'scheduled', outcome: 'ok', started_at: '2026-10-19T03:00:00Z', sessions_staged: 12, raw_bytes: 805_306_368, compressed_bytes: 188_743_680 },
  }),
  GetEntityCard: () => ({ facts: ['Prefers short answers.', 'Works in Rust and TypeScript.'] }),
  // One synthetic disabled job (hub #3120's Scheduled Jobs case).
  ListScheduledJobs: () => [{ id: '7d0c3a52-0000-4000-8000-000000000001', name: 'synthetic-nightly', message: 'run the suite',
    schedule: { recurrence: { type: 'EveryDays', value: 1 }, anchor: '2026-10-18T03:00:00Z' },
    last_fired_at: null, next_fire_at: '2026-10-20T03:00:00Z', enabled: false }],
  ListScheduledJobHolds: () => [],
  // Remote access with one synthetic device and project, neither allowed (hub #3112's dialog).
  RemoteGetStatus: () => ({
    enabled: true, canonical_host: 'fake-host.tailnet.test', url: null, unit_installed: true, gateway_running: true,
    serve_route_present: true, funnel_off: true, serve_pending_command: null,
    tailscale: { reachable: true, backend_state: 'Running', version: '1.0.0', pinned_version: '1.0.0', version_qualified: true },
    peers: [{ id: 'node-a', name: 'synthetic-phone', os: 'iOS', online: true, allowed: false }],
    projects: [{ id: 'p1', name: 'Fake project', exposed: false }],
    allow_answers: false, allow_messages: false, errors: [],
  }),
  OperatorTodoList: () => ({ open: [], recently_closed: [], truncated: false }),
  GetDaemonConfig: () => ({ title_model: 'haiku', provider_isolation: true, api_key: 'fake-should-never-reach-the-page' }),
  GetSession: (p) => session(p.session_id, 'Fake session', 'Running'),
  GetConversation: (p) => conversation(p.session_id),
  ListOperatorMessages: () => [],
  GetOperatorPause: () => ({ pause_level: 'none' }),
  GetSessionModelSwitchOptions: (p) => switchOptions(p.session_id),
  GetModelControlStatus: modelControlStatus,
  GetUsageStats: usageStats,
  // The manager surfaces: one global grant, one project manager with a pending decision and a saved policy, a dashboard and a budget.
  GetManagerTree: () => ({ rows: treeRows(), next_after: null, total_rows: 3, complete: true, global_grant_version: 3 }),
  GetHarnessManager: () => ({
    project_id: PROJECT_ID, manager_session_id: MANAGER_IDS.pmSeat, current_session_id: MANAGER_IDS.pmSeat, epic_ids: [SESSION_IDS.lead],
    scope_mode: 'selected', selected_epic_ids: [SESSION_IDS.lead], group_ids: [], row_version: 2, updated_at: T,
  }),
  GetHarnessManagerPolicy: managerPolicy,
  ListHarnessManagerScope: () => ({ rows: [{ id: SESSION_IDS.lead, title: 'Fake lead', kind: 'Epic', group_id: null, group_title: null }], next_after_id: null }),
  GetHarnessManagerState: (p) => statePage(p.query.section, (STATE_ROWS[p.query.section] ?? (() => []))()),
  GetGlobalManager: globalGrant,
  GetGlobalManagerWorkspace: workspace,
  GetGlobalManagerBudget: budgetSnapshot,
  GetGlobalDashboard: dashboard,
  ListIssuesPage: () => ({ rows: [issue(2, 'Second fake Issue', 'InProgress', 1), issue(1, 'First fake Issue', 'Open', 2)], next_cursor: null }),
  ListIssueDependencies: () => ({ items: [], next_cursor: null }),
  ListClaudeHooks: () => ({
    path: '/home/fake/.claude/settings.json', exists: true, revision: 'rev-1',
    hooks: { PreToolUse: [{ matcher: 'Bash', hooks: [{ type: 'command', command: 'echo fake-hook', timeout: 5 }] }] },
  }),
  ListCustomProviders: () => ({ providers: [{ id: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb', name: 'Fake gateway', base_url: 'https://gateway.example/v1', default_model: 'fake-1', api_key: { state: 'set', fingerprint: 'abcd1234' } }] }),
  ListCustomProviderRoutes: () => ({ routes: [
    { role: 'title_model', id: null, name: null, api_key: null },
    { role: 'memory_model_fallback', id: null, name: null, api_key: null },
    { role: 'dream_model', id: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb', name: 'Fake gateway', api_key: { state: 'set', fingerprint: 'abcd1234' } },
    { role: 'prompt_compile_model', id: null, name: null, api_key: null },
  ] }),
  ListMessageBridges: () => ({ bridges: [
    { kind: 'signal', path: '/home/fake/.rsi/signal.json', config: { enabled: false, account: '+10000000000', allow_from: '', working_dir: '/tmp/fake-project' } },
    { kind: 'imessage', path: '/home/fake/.rsi/imessage.json', config: null },
  ] }),
};

// ---------- scale fixtures (local #277) ----------

const SCALE_T0 = Date.parse('2026-10-01T00:00:00Z');
const stamp = (ms) => new Date(ms).toISOString().replace('Z', '000000Z');
const scaleId = (prefix, i) => `${prefix}-0000-4000-8000-${String(i).padStart(12, '0')}`;
const SCALE_STATUSES = ['Running', 'Completed', 'Completed', 'Failed', 'Interrupted', 'WaitingApproval', 'Completed'];

/**
 * `n` conversation events for session `id`, a User / ToolUse / ToolResult / Assistant cycle with a markdown message body.
 * Deterministic: the same `n` always gives the same events. `sequence` runs 1..n, as the daemon numbers them.
 */
export function longConversation(id, n) {
  return Array.from({ length: n }, (_, k) => {
    const base = { id: k + 1, session_id: id, sequence: k + 1, created_at: stamp(SCALE_T0 + k * 1000) };
    switch (k % 4) {
      case 1: return { ...base, event_type: 'ToolUse', role: 'Assistant', content: JSON.stringify({ name: 'Bash', input: { command: `rg pattern${k} src/` } }) };
      case 2: return { ...base, event_type: 'ToolResult', role: 'Tool', content: `match ${k}\n`.repeat(8) };
      default: return { ...base, event_type: 'Message', role: k % 4 === 0 ? 'User' : 'Assistant', content: `Message ${k}: ${'lorem ipsum dolor sit amet '.repeat(6)}\n\n- point one\n- point two \`code\`` };
    }
  });
}

/**
 * Fixtures for a big install: `sessions` sessions across `projects` projects (an Epic every 20th, two in three of the
 * rest parented to the Epic before them, statuses cycling through Running, Completed, Failed, Interrupted and
 * WaitingApproval), and one session (index `longIndex`) whose conversation has `events` events. Every other session keeps
 * the base fixtures' two-message conversation. `GetConversation` honours `since_sequence`, as the daemon does.
 *
 * Returns `{ fixtures, sessions, projects, longSessionId, counters }`; `fixtures` is the base set with the scale reads
 * replaced (pass it to `startFakeDaemon` or the audit `open`). `counters` counts the polling reads, so a scale run can
 * report how often the page asked: `{ ListSessions, GetConversation }`.
 */
export function scaleFixtures({ sessions: sessionCount = 800, events = 5000, projects: projectCount = 6, longIndex = 7, base = FIXTURES } = {}) {
  if (!Number.isInteger(sessionCount) || sessionCount < 1) throw new Error('sessions must be a positive integer');
  if (!Number.isInteger(events) || events < 0) throw new Error('events must be a non-negative integer');
  if (!Number.isInteger(projectCount) || projectCount < 1) throw new Error('projects must be a positive integer');
  const li = Math.min(Math.max(longIndex, 0), sessionCount - 1);
  const projects = Array.from({ length: projectCount }, (_, i) => ({ id: scaleId('aaaaaaaa', i), name: `Project ${i}`, path: `/tmp/p${i}`, color: '#7aa2f7' }));
  const sessions = Array.from({ length: sessionCount }, (_, i) => {
    const epic = i % 20 === 0;
    const parent = !epic && i % 3 !== 0 ? scaleId('bbbbbbbb', i - (i % 20)) : undefined;
    const t = stamp(SCALE_T0 + i * 60000);
    return {
      id: scaleId('bbbbbbbb', i), status: epic ? 'Running' : SCALE_STATUSES[i % SCALE_STATUSES.length], provider: 'Claude',
      session_kind: epic ? 'Epic' : 'Agent', title: `Session ${i} ${epic ? 'lead' : 'worker'} for task ${i * 7}`, query: `task ${i}`,
      working_dir: '/tmp', model: 'opus', project_id: projects[i % projectCount].id, created_at: t, updated_at: t, ...(parent ? { parent_id: parent } : {}),
    };
  });
  const longSessionId = sessions[li].id;
  const long = longConversation(longSessionId, events);
  const counters = { ListSessions: 0, GetConversation: 0 };
  const fixtures = {
    ...base,
    ListProjects: () => [...projects, HOME_PROJECT],
    ListSessions: () => { counters.ListSessions++; return sessions; },
    GetSession: (p) => sessions.find((s) => s.id === p.session_id) ?? base.GetSession(p),
    GetConversation: (p) => {
      counters.GetConversation++;
      const all = p.session_id === longSessionId ? long : base.GetConversation(p);
      return p.since_sequence == null ? all : all.filter((e) => e.sequence > p.since_sequence);
    },
  };
  return { fixtures, sessions, projects, longSessionId, counters };
}

/**
 * A fixture's answer as a JSON-RPC response (local #313). A fixture may return a value, a Promise (answers late), or
 * `{ error: { code, message, data? } }` to answer a JSON-RPC error; one that throws (or rejects) answers -32603 with its message. The
 * fake never dies because of a fixture.
 */
export async function fixtureResponse(f, req) {
  try {
    const result = await f(req.params ?? {});
    const e = result?.error;
    if (e && typeof e === 'object' && !Array.isArray(e) && Object.keys(result).length === 1 && typeof e.message === 'string') {
      return { jsonrpc: '2.0', id: req.id, error: { code: Number.isInteger(e.code) ? e.code : -32000, message: e.message, ...(e.data !== undefined ? { data: e.data } : {}) } };
    }
    return { jsonrpc: '2.0', id: req.id, result };
  } catch (err) {
    return { jsonrpc: '2.0', id: req.id, error: { code: -32603, message: `fixture ${req.method} threw: ${String(err?.message ?? err)}` } };
  }
}

/** Start the fake. `received` collects `{method, params}` in arrival order. */
export function startFakeDaemon(fixtures = FIXTURES) {
  // A short base: a Unix socket path is limited to about 104 bytes (macOS), and sandbox TMPDIRs are deep.
  const dir = mkdtempSync(join(existsSync('/tmp') ? '/tmp' : tmpdir(), 'rsi-fake-'));
  const socket = join(dir, 'daemon.sock');
  const received = [];
  const server = createServer((conn) => {
    let buf = '';
    conn.on('data', (d) => {
      buf += d.toString('utf8');
      const nl = buf.indexOf('\n');
      if (nl < 0) return;
      let req;
      try { req = JSON.parse(buf.slice(0, nl)); } catch { conn.end(); return; }
      buf = '';
      received.push({ method: req.method, params: req.params });
      const f = Object.hasOwn(fixtures, req.method) ? fixtures[req.method] : null;
      const answer = f
        ? fixtureResponse(f, req)
        : Promise.resolve({ jsonrpc: '2.0', id: req.id, error: { code: -32601, message: `Method not found: ${req.method}` } });
      answer.then((res) => { if (!conn.destroyed) conn.end(`${JSON.stringify(res)}\n`); });
    });
    conn.on('error', () => {});
  });
  return new Promise((resolve) => server.listen(socket, () => resolve({
    socket,
    received,
    stop: () => new Promise((done) => { server.closeAllConnections?.(); server.close(() => { rmSync(dir, { recursive: true, force: true }); done(); }); }),
  })));
}
