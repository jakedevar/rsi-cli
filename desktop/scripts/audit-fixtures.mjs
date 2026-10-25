// Fake-daemon fixtures with enough rows to exercise a keyboard list (local #327). The default FIXTURES (smoke-fake.mjs) keep one
// row or none for most lists, so j / k, g / G and x always "do nothing" there. `keyFixtures(base)` is the walk's fixtures
// (walkFixtures: files, to-dos) plus three rows for the Trash, Scheduled jobs, the manager scope, the manager's pending decisions,
// the source-worktree settlement runs and the completed sessions that Recent completions lists. Notifications are not a daemon
// read: `seedNotices` records them in the page. Pure data, no browser: tests/audit-fixtures.test.mjs reads the shapes.
import { walkFixtures } from './audit-listbox-walk.mjs';

const T = '2026-01-02T03:04:05.000000000Z';
const PROJECT_ID = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';
const id = (prefix, n) => `${prefix}-0000-4000-8000-${String(n).padStart(12, '0')}`;
const session = (sid, title, status, extra = {}) => ({
  id: sid, status, provider: 'Claude', session_kind: 'Agent', title, query: `${title} (query)`, working_dir: '/tmp/fake-project',
  model: 'default', project_id: PROJECT_ID, created_at: T, updated_at: T, ...extra,
});

/** `n` rows from a row factory (1-based). */
const rows = (n, make) => Array.from({ length: n }, (_, i) => make(i + 1));

/** Extra pending manager decisions (the base fixture has one); the state page keeps its envelope. */
const extraDecisions = (n) => rows(n, (k) => ({
  type: 'decision', key: `decision-extra-${k}`, session_id: '22222222-2222-4222-8222-222222222222', status: 'pending',
  question: `Extra decision ${k}: ship the fix or hold it?`,
  options: [{ label: 'A', detail: 'Ship', recommended: true }, { label: 'B', detail: 'Hold', recommended: false }],
  target_digest: `sha256:${String(k).repeat(64).slice(0, 64)}`, row_version: 1, created_at: T, updated_at: T,
}));

// ---------- the recursive DAG browser (local #336) ----------

const DAG_CAPS = ['recursive_dag_inspection', 'recursive_dag_cancellation_control', 'recursive_dag_recovery_control', 'recursive_dag_recovery_status', 'recursive_dag_scheduler_control',
  'recursive_dag_run_inspection', 'recursive_dag_live_status_inspection', 'recursive_dag_live_validation_inspection', 'recursive_dag_artifact_list_pagination',
  'recursive_dag_artifact_lookup', 'recursive_dag_artifact_preview_inspection'];
const G1 = id('d0000001', 1);
const G2 = id('d0000001', 2);
const dagTask = (n, parent, title, status) => ({
  id: id('d0000002', n), graph_id: G1, parent_task_id: parent === null ? null : id('d0000002', parent), title, status,
  objective: `${title}: keep the checks green`, verification_strategy: n % 2 ? 'run the unit tests' : null, blocked_reason: status === 'blocked' ? 'waits for task 2' : null,
  created_at: `2026-10-20T10:0${n}:00.000000000Z`,
});
const RUNS = rows(2, (k) => ({
  id: id('d0000003', k), graph_id: G1, status: k === 1 ? 'running' : 'succeeded', step_count: 4 * k, max_steps: 8, started_at: `2026-10-20T11:0${k}:00.000000000Z`,
  completed_at: k === 1 ? null : '2026-10-20T11:30:00.000000000Z', lease_heartbeat_at: T, lease_expires_at: '2026-10-21T00:00:00.000000000Z',
}));
const ATTEMPTS = rows(2, (k) => ({
  live_attempt_id: id('d0000004', k), id: id('d0000004', k), graph_id: G1, task_id: id('d0000002', k + 1), title: `Live attempt ${k}`, session_id: id('eeeeeeee', k), status: 'running',
  heartbeat: { live_attempt_id: id('d0000004', k), heartbeat_at: T, heartbeat_expires_at: '2026-10-21T00:00:00.000000000Z' },
  latest_validation: { validation_id: id('d0000005', k), task_id: id('d0000002', k + 1), live_attempt_id: id('d0000004', k), status: 'passed' },
}));

/**
 * Fake-daemon answers for the recursive DAG browser: the capabilities it needs, two graphs (the second quarantined), the first with a five-task
 * tree, two scheduler runs, a cancellation request, a deferred graph, a heartbeat, two live attempts, a validation and three artifacts, so every
 * panel has rows to move over and every control is enabled. Pure data.
 */
export function dagFixtures() {
  const graph = (gid, title, extra = {}) => ({ id: gid, title, status: 'active', execution_mode: 'fake', project_id: PROJECT_ID, root_task_id: id('d0000002', 1), updated_at: T, max_depth: 3, max_fanout: 4, ...extra });
  return {
    GetDaemonCapabilities: () => Object.fromEntries(DAG_CAPS.map((c) => [c, true])),
    ListRecursiveTaskGraphs: () => [graph(G1, 'Audit DAG graph', { updated_at: '2026-10-20T12:00:00.000000000Z' }), graph(G2, 'Quarantined DAG graph', { quarantined_at: '2026-10-19T09:00:00.000000000Z', quarantine_reason: 'malformed task row', updated_at: '2026-10-19T09:00:00.000000000Z' })],
    GetRecursiveTaskGraph: () => ({
      graph: graph(G1, 'Audit DAG graph', { updated_at: '2026-10-20T12:00:00.000000000Z' }),
      nodes: [dagTask(1, null, 'Root plan', 'running'), dagTask(2, 1, 'Write the parser', 'succeeded'), dagTask(3, 1, 'Wire the browser', 'ready'), dagTask(4, 3, 'Add the tests', 'blocked'), dagTask(5, 3, 'Review the diff', 'failed')],
      edges: [{ from_task_id: id('d0000002', 2), to_task_id: id('d0000002', 3) }, { from_task_id: id('d0000002', 3), to_task_id: id('d0000002', 4) }],
      attempts: [],
    }),
    GetRecursiveDagOperationalStatus: () => ({ active_run: RUNS[0], latest_run: RUNS[0], running_attempts: [], open_cancellation_requests: [], active_live_attempts: ATTEMPTS, active_live_heartbeats: [], active_live_interrupts: [], live_recovery_pending: [], latest_live_validations: [] }),
    ListRecursiveSchedulerRuns: () => RUNS,
    GetRecursiveSchedulerRun: () => ({ ...RUNS[0], cancellation_requests: [], live_interrupts: [], live_heartbeat_states: [], live_attempts: ATTEMPTS, latest_live_validations: [] }),
    ListRecursiveSchedulerRunEvents: () => rows(3, (k) => ({ id: id('d0000006', k), event_type: ['scheduled', 'started', 'stepped'][k - 1], message: `run event ${k}`, created_at: T })),
    ListRecursiveCancellationRequests: () => [{ id: id('d0000007', 1), graph_id: G1, reason: 'operator asked to stop', requested_at: T, status: 'open' }],
    GetRecursiveRecoveryStatus: () => ({ deferred_graph_count: 1, deferred_graphs: [{ graph_id: G2, reason: 'waiting for the budget' }], latest_pass: { pass_id: id('d0000008', 1), checked: 2, recovered: 0, deferred: 1, errors: 0 }, graph_status: { graph_id: G1 } }),
    ListStaleRecursiveLiveAttemptHeartbeats: () => [{ live_attempt_id: id('d0000004', 9), stale_reason: 'no heartbeat for 10 minutes', heartbeat_at: T, heartbeat_expires_at: T }],
    GetRecursiveLiveRecoveryStatus: () => ({ warnings: [], live_attempts: [], heartbeat_states: [], deferred_graph: null }),
    ListRecursiveLiveAttempts: () => ATTEMPTS,
    ListRecursiveLiveOutputValidationResults: () => ATTEMPTS.map((a) => ({ ...a.latest_validation, graph_id: G1, issues: [] })),
    ListRecursiveExecutionArtifactSummaries: () => ({
      items: rows(3, (k) => ({ id: id('d0000009', k), graph_id: G1, label: `artifact-${k}.md`, uri: `file:///tmp/fake-project/artifact-${k}.md`, created_at: T, updated_at: T })),
      has_more: false, next_cursor: null, warnings: [],
    }),
  };
}

/** Three saved workflows (all open the same document: a plan that branches into two nodes) for Graph review's saved-workflow picker; `GetWorkflowDefinition` answers the same document. */
const WORKFLOW = {
  workflow: { id: id('d0000010', 1), title: 'Audit workflow', stage: 'draft', artifact_path: null, project_id: PROJECT_ID, created_at: T, updated_at: T },
  definition: {
    version: '1.0', name: 'Audit workflow', description: 'three nodes for the key sweep',
    nodes: ['Plan', 'Build', 'Verify'].map((name, i) => ({ id: `n${i + 1}`, name, type: i === 1 ? 'topology' : 'action', instructions: `${name} the change`, inputs: [], outputs: [], tags: [] })),
    edges: [{ source: 'n1', target: 'n2' }, { source: 'n1', target: 'n3' }],
  },
};

/** The fixtures with several rows for each keyboard list the issue names. `base` is `lib.FIXTURES`. */
export function keyFixtures(base) {
  const walk = walkFixtures(base);
  return {
    ...walk,
    ...dagFixtures(),
    ReadProjectFile: (p) => {
      const content = Array.from({ length: 160 }, (_, i) => `line ${i + 1}: audit text for ${p.path}`).join('\n') + '\n';
      return { root: '/tmp/fake-project', path: p.path, size: content.length, binary: false, content, truncated: false, start_line: 1, end_line: 160, version: 'v1:1:1:audit' };
    },
    ListWorkflows: () => rows(3, (k) => ({ ...WORKFLOW.workflow, id: id('d0000010', k), title: k === 1 ? 'Audit workflow' : `Audit workflow ${k}` })),
    GetWorkflowDefinition: () => ({ document: WORKFLOW }),
    ListSessions: async (p, ...rest) => [
      ...(await walk.ListSessions(p, ...rest)),
      session(id('eeeeeeee', 1), 'Fake completed session two', 'Completed', { query: 'summarise the audit' }),
      session(id('eeeeeeee', 2), 'Fake completed session three', 'Completed', { query: 'tidy the release notes' }),
    ],
    ListLabels: () => rows(3, (k) => ({ id: id('1abe1000', k), name: `label-${k}`, description: `Audit label ${k}`, project_id: null, color: ['#7aa2f7', '#9ece6a', '#f7768e'][k - 1] })),
    ListDeletedSessions: () => rows(3, (k) => session(id('66666666', k), `Fake deleted worker ${k}`, 'Deleted', { updated_at: T })),
    ListScheduledJobs: () => rows(3, (k) => ({
      id: id('7d0c3a52', k), name: `synthetic-job-${k}`, message: `run suite ${k}`,
      schedule: { recurrence: { type: 'EveryDays', value: k }, anchor: '2026-10-18T03:00:00Z' },
      last_fired_at: null, next_fire_at: '2026-10-20T03:00:00Z', enabled: k !== 2,
    })),
    ListHarnessManagerScope: () => ({
      rows: rows(3, (k) => ({ id: id('11111111', k), title: `Fake lead ${k}`, kind: 'Epic', group_id: null, group_title: null })),
      next_after_id: null,
    }),
    GetHarnessManagerState: async (p, ...rest) => {
      const page = await walk.GetHarnessManagerState(p, ...rest);
      return p.query.section === 'decisions' ? { ...page, rows: [...page.rows, ...extraDecisions(2)] } : page;
    },
    ListSourceWorktreeCohorts: () => rows(3, (k) => ({ repository_identity: `git:fake-repo-${k}`, canonical_repo_dir: `/tmp/fake-project-${k}`, live_roots: k, terminal_roots: 5 })),
    ListSatellitePeers: async (p, ...rest) => {
      const page = await walk.ListSatellitePeers(p, ...rest);
      const [first] = page.peers;
      return { ...page, peers: [first, ...[2, 3].map((k) => ({ ...first, config: { ...first.config, peer_id: id('abababab', k), label: `mac-mini-${k}` } }))] };
    },
    RemoteGetStatus: async (p, ...rest) => {
      const st = await walk.RemoteGetStatus(p, ...rest);
      return {
        ...st,
        peers: rows(3, (k) => ({ id: `node-${k}`, name: `synthetic-phone-${k}`, os: 'iOS', online: k !== 2, allowed: false })),
        projects: rows(3, (k) => ({ id: `p${k}`, name: `Fake project ${k}`, exposed: false })),
      };
    },
    ListSourceWorktreeSettlementRuns: () => rows(3, (k) => ({
      run_id: id('ffffffff', k), repository_identity: `git:fake-repo-${k}`, state: k === 2 ? 'partial' : 'completed',
      counts: { observed: 7, eligible: 5, retained: 2, settled: 5, refused: 0, recovery_required: 0, unattempted: 0 }, created_at: `2026-10-1${k}T09:00:00.000000000Z`,
    })),
  };
}

/**
 * Answers for the shim-local commands (`open({ local })`): the model catalog with an effort ladder per model, which the switch dialog needs to
 * step effort (the default `discover_models` answer reports none, so h / l had nothing to move through).
 */
export const KEY_LOCAL = {
  discover_models: () => ({
    models: [['opus', 'Opus'], ['sonnet', 'Sonnet'], ['haiku', 'Haiku']],
    effort_capabilities: ['opus', 'sonnet', 'haiku'].map((model) => ({ model, supported_efforts: ['low', 'medium', 'high'], default_effort: 'medium' })),
  }),
};

/** Record `n` notices in the page (the Notifications list is the page's own, not a daemon read). Notices of one kind inside 500 ms fold into one, so they are spaced out. */
export const seedNotices = (page, n = 3) => page.eval(`(async () => { const d = await import('./dom.js'); for (let i = 1; i <= ${n}; i++) { d.recordNotice('Audit notice ' + i, 'detail ' + i); await new Promise((r) => setTimeout(r, 560)); } return ${n}; })()`);
