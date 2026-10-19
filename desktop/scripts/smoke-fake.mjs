// A fake rsid for the desktop smoke: a Unix-socket JSON-RPC server (one request line in, one
// response line out, as src-tauri/src/daemon.rs speaks it) with canned read results. It records every
// method it receives, so a test can assert that nothing but reads ever reached a daemon.
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export const PROJECT_ID = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';
export const SESSION_IDS = {
  lead: '11111111-1111-4111-8111-111111111111',
  worker: '22222222-2222-4222-8222-222222222222',
  done: '33333333-3333-4333-8333-333333333333',
};
const T = '2026-01-02T03:04:05.000000000Z';

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

const conversation = (id) => [
  { id: 1, session_id: id, sequence: 1, event_type: 'Message', role: 'User', created_at: T, content: 'Fix the list view.' },
  { id: 2, session_id: id, sequence: 2, event_type: 'Message', role: 'Assistant', created_at: T, content: 'I fixed the list view and added a test.' },
];

/** The canned read results. Keys are rsid RPC method names. */
export const FIXTURES = {
  GetHealthStatus: () => ({ status: 'ok', version: 'fake' }),
  ListProjects: () => [{ id: PROJECT_ID, name: 'Fake project', path: '/tmp/fake-project', color: '#7aa2f7' }],
  ListSessions: () => [
    session(SESSION_IDS.lead, 'Fake lead', 'Running', { session_kind: 'Epic' }),
    session(SESSION_IDS.worker, 'Fake worker', 'Running', { parent_id: SESSION_IDS.lead }),
    session(SESSION_IDS.done, 'Fake finished worker', 'Failed', { parent_id: SESSION_IDS.lead }),
  ],
  ListArchivedSessions: () => [],
  ListLabels: () => [],
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
      received.push({ method: req.method, params: req.params });
      const f = fixtures[req.method];
      const res = f
        ? { jsonrpc: '2.0', id: req.id, result: f(req.params ?? {}) }
        : { jsonrpc: '2.0', id: req.id, error: { code: -32601, message: `Method not found: ${req.method}` } };
      conn.end(`${JSON.stringify(res)}\n`);
    });
    conn.on('error', () => {});
  });
  return new Promise((resolve) => server.listen(socket, () => resolve({
    socket,
    received,
    stop: () => new Promise((done) => server.close(() => { rmSync(dir, { recursive: true, force: true }); done(); })),
  })));
}
