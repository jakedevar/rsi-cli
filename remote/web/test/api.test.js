import test from 'node:test';
import assert from 'node:assert/strict';
import { createApi, limits } from '../api.js';

const NONCE = 'a'.repeat(64);
const CSRF = 'b'.repeat(64);
const KNOWN = (value) => ({ state: 'known', value });
const UNKNOWN = (label) => ({ state: 'unknown', label });

const jsonResponse = (value, status = 200) => ({
  ok: status >= 200 && status < 300,
  status,
  headers: new Headers({ 'content-type': 'application/json' }),
  text: async () => JSON.stringify(value),
});
const emptyResponse = (status) => ({
  ok: status >= 200 && status < 300,
  status,
  headers: new Headers(),
  text: async () => '',
});

const summary = (over = {}) => ({
  id: 's-1',
  project_id: 'p-1',
  parent_id: null,
  continued_from: null,
  kind: KNOWN('Standard'),
  own_title: 'Build the remote UI',
  provider: KNOWN('Codex'),
  status: KNOWN('Running'),
  updated_at: '2026-09-26T12:30:00Z',
  attention: { requires_local_action: false, incomplete: false, live_signals_lower_bound: '0' },
  ...over,
});

const decisionField = (text, state = 'complete') => ({ text, state, source_field: 'method', source_extent: 'full_field', observed_bytes: null });
const genericDecision = (over = {}) => ({
  id: 'question:1',
  identity_class: 'slot',
  kind: 'generic_questions',
  publication_state: KNOWN('published'),
  closure_state: 'open',
  delivery_state: 'unknown',
  source_observations: [],
  omitted_source_observations: '0',
  disagreement: false,
  display: { kind: 'generic_questions' },
  questions: [],
  omitted_questions: '0',
  details_state: 'complete',
  requires_local_action: true,
  can_answer: false,
  ...over,
});
const nativeDecision = (over = {}) => ({
  ...genericDecision({ id: 'native:2', identity_class: 'publication', kind: 'native_approval' }),
  display: { kind: 'native_approval', method: decisionField('Allow network access'), description: decisionField('The tool wants to reach example.com') },
  ...over,
});
const legacyDecision = (over = {}) => ({
  ...genericDecision({ id: 'legacy:3', identity_class: 'legacy', kind: 'legacy_approval' }),
  display: { kind: 'legacy_approval', tool_name: decisionField('Bash') },
  ...over,
});

const historyEvent = (over = {}) => ({
  id: '42',
  sequence: 1,
  kind: KNOWN('Message'),
  role: KNOWN('Assistant'),
  created_at: '2026-09-26T12:30:00Z',
  text: 'hello <b>world</b>',
  content_bytes: '16',
  truncated: false,
  tool_name: null,
  tool_pair_key: null,
  tool_id_display: null,
  pairing_state: 'missing',
  content_state: 'complete',
  ...over,
});

function makeGateway(config = {}) {
  const calls = [];
  let remaining401 = config.data401Once ?? 0;
  let remainingDecisions401 = config.decisions401Once ?? 0;
  let remainingOlder401 = config.olderHistory401Once ?? 0;
  const fetchImpl = async (path, init = {}) => {
    calls.push({ path, init });
    const [pathname, search = ''] = path.split('?');
    const params = new URLSearchParams(search);
    if (path === '/auth/bootstrap') {
      if (config.bootstrapStatus) return emptyResponse(config.bootstrapStatus);
      if (config.denyBootstrapOnce > 0) { config.denyBootstrapOnce -= 1; return emptyResponse(403); }
      return jsonResponse({ nonce: NONCE });
    }
    if (path === '/auth/session') {
      if (config.sessionStatus) return emptyResponse(config.sessionStatus);
      return jsonResponse({ csrf: CSRF });
    }
    if (path === '/auth/logout') return emptyResponse(config.logoutStatus ?? 204);
    if (config.dataStatus) return emptyResponse(config.dataStatus);
    if (config.data401) return emptyResponse(401);
    if (remaining401 > 0) { remaining401 -= 1; return emptyResponse(401); }
    if (pathname === '/api/v1/projects') return jsonResponse({ items: config.projects ?? [] });
    let match = pathname.match(/^\/api\/v1\/projects\/([^/]+)\/sessions$/);
    if (match) {
      const projectId = decodeURIComponent(match[1]);
      const cursor = params.get('cursor');
      if (config.sessionPages?.[projectId]) {
        return jsonResponse(config.sessionPages[projectId](cursor) ?? { items: [] });
      }
      return jsonResponse({ items: (config.sessions ?? {})[projectId] ?? [] });
    }
    match = pathname.match(/^\/api\/v1\/projects\/([^/]+)\/sessions\/([^/]+)\/decisions$/);
    if (match) {
      if (config.decisionsStatus) return emptyResponse(config.decisionsStatus);
      if (remainingDecisions401 > 0) { remainingDecisions401 -= 1; return emptyResponse(401); }
      return jsonResponse({ items: config.decisions?.[decodeURIComponent(match[2])] ?? [] });
    }
    match = pathname.match(/^\/api\/v1\/projects\/([^/]+)\/sessions\/([^/]+)\/history$/);
    if (match) {
      const sessionId = decodeURIComponent(match[2]);
      const before = params.get('before');
      if (before !== null && remainingOlder401 > 0) { remainingOlder401 -= 1; return emptyResponse(401); }
      if (before !== null && config.olderHistory?.[sessionId]) {
        return jsonResponse(config.olderHistory[sessionId][before] ?? { items: [] });
      }
      return jsonResponse({ items: config.history?.[sessionId] ?? [] });
    }
    match = pathname.match(/^\/api\/v1\/projects\/([^/]+)\/sessions\/([^/]+)$/);
    if (match) return jsonResponse({ item: config.detail?.[decodeURIComponent(match[2])] ?? null });
    return emptyResponse(404);
  };
  return { fetchImpl, calls, paths: () => calls.map((call) => call.path) };
}

const project = (id, name = `Project ${id}`) => ({ id, name });
const detail = (value) => ({ summary: value });

test('bootstrap and session are established before the first data GET', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [summary()] },
    detail: { 's-1': detail(summary()) },
    history: { 's-1': [historyEvent()] },
  });
  const api = createApi(gw.fetchImpl);
  assert.deepEqual(await api.listSessions(), [
    { id: 's-1', title: 'Build the remote UI', status: 'Running', provider: 'Codex', updatedAt: '2026-09-26T12:30:00Z', needsYou: false, attentionIncomplete: false },
  ]);
  assert.deepEqual(gw.paths(), ['/auth/bootstrap', '/auth/session', '/api/v1/projects', '/api/v1/projects/p-1/sessions']);
});

test('getSession routes through the remembered project and maps events', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [summary()] },
    detail: { 's-1': detail(summary()) },
    history: { 's-1': [historyEvent(), historyEvent({ id: '43', kind: KNOWN('ToolUse'), role: null, tool_name: 'Read', text: 'src/main.rs' })] },
  });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const loaded = await api.getSession('s-1');
  assert.deepEqual(loaded.session, { id: 's-1', title: 'Build the remote UI', status: 'Running', provider: 'Codex', updatedAt: '2026-09-26T12:30:00Z', needsYou: false, attentionIncomplete: false });
  assert.deepEqual(loaded.events.map((event) => event.kind), ['assistant', 'tool']);
  assert.equal(loaded.events[1].toolName, 'Read');
  assert.match(loaded.events[1].text, /^Read: /);
  assert.ok(gw.paths().includes('/api/v1/projects/p-1/sessions/s-1'));
  assert.ok(gw.paths().includes('/api/v1/projects/p-1/sessions/s-1/history'));
});

test('an unknown session ID asks for a list refresh', async () => {
  const gw = makeGateway({ projects: [], sessions: {} });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  await assert.rejects(api.getSession('s-9'), /Refresh the session list/);
});

test('CSRF is sent only on logout and kept out of data reads', async () => {
  const gw = makeGateway({ projects: [project('p-1')], sessions: { 'p-1': [summary()] }, detail: { 's-1': detail(summary()) }, history: { 's-1': [] } });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  for (const call of gw.calls) {
    assert.equal(call.init.headers?.['x-rsi-csrf'], undefined);
  }
  await api.logout();
  const logout = gw.calls.find((call) => call.path === '/auth/logout');
  assert.equal(logout.init.method, 'POST');
  assert.equal(logout.init.headers['x-rsi-csrf'], CSRF);
  assert.equal(logout.init.body, undefined);
  const before = gw.calls.length;
  await api.logout();
  assert.equal(gw.calls.length, before, 'a second logout with no session makes no request');
});

test('one 401 re-bootstraps and retries; a second 401 fails', async () => {
  const gw = makeGateway({ projects: [], data401Once: 1 });
  const api = createApi(gw.fetchImpl);
  assert.deepEqual(await api.listSessions(), []);
  assert.equal(gw.paths().filter((path) => path === '/auth/bootstrap').length, 2);
  assert.equal(gw.paths().filter((path) => path === '/api/v1/projects').length, 2);

  const always = makeGateway({ data401: true });
  const failing = createApi(always.fetchImpl);
  await assert.rejects(failing.listSessions(), /session has expired/);
  assert.equal(always.paths().filter((path) => path === '/api/v1/projects').length, 2);
});

test('403 and 502 surface distinct, actionable errors', async () => {
  const forbidden = createApi(makeGateway({ dataStatus: 403 }).fetchImpl);
  await assert.rejects(forbidden.listSessions(), /gateway denied this request/);
  const unavailable = createApi(makeGateway({ dataStatus: 502 }).fetchImpl);
  await assert.rejects(unavailable.listSessions(), /RSI daemon is unavailable/);
});

test('sessions fan out across projects and keep id->project routing', async () => {
  const gw = makeGateway({
    projects: [project('p-1'), project('p-2')],
    sessions: { 'p-1': [summary()], 'p-2': [summary({ id: 's-2', project_id: 'p-2', own_title: 'Second' })] },
    detail: { 's-2': detail(summary({ id: 's-2', project_id: 'p-2', own_title: 'Second' })) },
    history: { 's-2': [historyEvent()] },
  });
  const api = createApi(gw.fetchImpl);
  const sessions = await api.listSessions();
  assert.deepEqual(sessions.map((session) => session.id), ['s-1', 's-2']);
  await api.getSession('s-2');
  assert.ok(gw.paths().includes('/api/v1/projects/p-2/sessions/s-2'));
  assert.ok(!gw.paths().includes('/api/v1/projects/p-2/sessions/s-1'));
});

test('unknown and new status, provider and event kinds map to safe values', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [
      summary({ id: 's-1', status: UNKNOWN('Paused'), provider: UNKNOWN('Mystery') }),
      summary({ id: 's-2', status: KNOWN('Deleted'), provider: KNOWN('Harness') }),
    ] },
    detail: { 's-1': detail(summary({ id: 's-1', status: UNKNOWN('Paused'), provider: UNKNOWN('Mystery') })) },
    history: { 's-1': [
      historyEvent({ id: '1', kind: UNKNOWN('Weird'), role: null }),
      historyEvent({ id: '2', kind: KNOWN('Thinking'), role: null }),
      historyEvent({ id: '3', kind: KNOWN('Message'), role: KNOWN('User') }),
      historyEvent({ id: '4', kind: KNOWN('System'), role: null }),
    ] },
  });
  const api = createApi(gw.fetchImpl);
  const sessions = await api.listSessions();
  assert.deepEqual(sessions.map((session) => [session.status, session.provider]), [
    ['Archived', 'Local'],
    ['Deleted', 'Harness'],
  ]);
  const loaded = await api.getSession('s-1');
  assert.deepEqual(loaded.events.map((event) => event.kind), ['system', 'thinking', 'user', 'system']);
  assert.equal(loaded.events[3].text, historyEvent().text, 'text is inert data');
});

test('duplicate identities, oversized collections and oversized text still fail', async () => {
  const duplicate = createApi(makeGateway({ projects: [project('p-1'), project('p-2')], sessions: { 'p-1': [summary()], 'p-2': [summary()] } }).fetchImpl);
  await assert.rejects(duplicate.listSessions(), /duplicate identity/);

  const manyProjects = Array.from({ length: limits.MAX_PROJECTS + 1 }, (_value, index) => project(`p-${index}`));
  const tooManyProjects = createApi(makeGateway({ projects: manyProjects }).fetchImpl);
  await assert.rejects(tooManyProjects.listSessions(), /oversized project list/);

  const manySessions = Array.from({ length: limits.MAX_SESSIONS + 1 }, (_value, index) => summary({ id: `s-${index}` }));
  const tooManySessions = createApi(makeGateway({ projects: [project('p-1')], sessions: { 'p-1': manySessions } }).fetchImpl);
  await assert.rejects(tooManySessions.listSessions(), /oversized session list/);

  const tooMuchText = createApi(makeGateway({
    projects: [project('p-1')], sessions: { 'p-1': [summary()] },
    detail: { 's-1': detail(summary()) },
    history: { 's-1': [historyEvent({ text: 'x'.repeat(limits.MAX_TEXT_BYTES + 1) })] },
  }).fetchImpl);
  await tooMuchText.listSessions();
  await assert.rejects(tooMuchText.getSession('s-1'), /event text/);
});

test('history keeps only the most recent window of events', async () => {
  const events = Array.from({ length: limits.MAX_EVENTS + 5 }, (_value, index) => historyEvent({ id: String(index) }));
  const gw = makeGateway({ projects: [project('p-1')], sessions: { 'p-1': [summary()] }, detail: { 's-1': detail(summary()) }, history: { 's-1': events } });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const loaded = await api.getSession('s-1');
  assert.equal(loaded.events.length, limits.MAX_EVENTS);
  assert.equal(loaded.events[0].id, '5', 'the oldest events are dropped');
});

test('oversized response bodies and unreachable hosts fail clearly', async () => {
  const huge = createApi(async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new Uint8Array(limits.MAX_BODY_BYTES + 1)); },
  }), { headers: { 'Content-Type': 'application/json' } }));
  await assert.rejects(huge.listSessions(), /too large/);

  const offline = createApi(async () => { throw new TypeError('offline'); });
  await assert.rejects(offline.listSessions(), /Check the connection and retry/);
});

test('attention maps to needsYou and attentionIncomplete booleans', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [
      summary({ id: 's-1', attention: { requires_local_action: true, incomplete: true, live_signals_lower_bound: '2' } }),
      summary({ id: 's-2', attention: { requires_local_action: false, incomplete: false, live_signals_lower_bound: '0' } }),
    ] },
  });
  const api = createApi(gw.fetchImpl);
  const sessions = await api.listSessions();
  assert.deepEqual(sessions.map((session) => [session.needsYou, session.attentionIncomplete]), [[true, true], [false, false]]);
});

test('a missing or malformed attention object fails the session mapping', async () => {
  const missing = createApi(makeGateway({ projects: [project('p-1')], sessions: { 'p-1': [summary({ attention: null })] } }).fetchImpl);
  await assert.rejects(missing.listSessions(), /session attention/);
  const malformed = createApi(makeGateway({ projects: [project('p-1')], sessions: { 'p-1': [summary({ attention: { requires_local_action: 'yes', incomplete: false } })] } }).fetchImpl);
  await assert.rejects(malformed.listSessions(), /session attention/);
});

test('getSession maps all three decision kinds and marks closed decisions', async () => {
  const attention = { requires_local_action: true, incomplete: false, live_signals_lower_bound: '1' };
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [summary({ attention })] },
    detail: { 's-1': detail(summary({ attention })) },
    history: { 's-1': [] },
    decisions: { 's-1': [
      genericDecision(),
      nativeDecision(),
      legacyDecision(),
      nativeDecision({ id: 'native:4', closure_state: 'closed' }),
    ] },
  });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const loaded = await api.getSession('s-1');
  assert.equal(loaded.decisionsError, '');
  assert.deepEqual(loaded.decisions.map((decision) => [decision.id, decision.kind, decision.title, decision.detail, decision.open]), [
    ['question:1', 'question', 'A question is waiting', '', true],
    ['native:2', 'approval', 'Allow network access', 'The tool wants to reach example.com', true],
    ['legacy:3', 'approval', 'Bash', '', true],
    ['native:4', 'approval', 'Allow network access', 'The tool wants to reach example.com', false],
  ]);
});

test('truncated and unavailable display fields are marked, and decisions are capped', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [summary()] },
    detail: { 's-1': detail(summary()) },
    history: { 's-1': [] },
    decisions: { 's-1': [
      nativeDecision({ id: 'native:5', display: { kind: 'native_approval', method: decisionField('Allow network', 'truncated'), description: decisionField('', 'unavailable') } }),
      legacyDecision({ id: 'legacy:6', display: { kind: 'legacy_approval', tool_name: decisionField('Bash', 'unavailable') } }),
      ...Array.from({ length: 20 }, (_value, index) => genericDecision({ id: `question:${index + 10}` })),
    ] },
  });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const loaded = await api.getSession('s-1');
  assert.equal(loaded.decisions.length, limits.MAX_DECISIONS);
  assert.equal(loaded.decisions[0].title, 'Allow network …');
  assert.equal(loaded.decisions[0].detail, 'Unavailable');
  assert.equal(loaded.decisions[1].title, 'Unavailable');
});

test('a failed decisions fetch degrades to null without hiding the detail', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [summary()] },
    detail: { 's-1': detail(summary()) },
    history: { 's-1': [historyEvent()] },
    decisionsStatus: 502,
  });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const loaded = await api.getSession('s-1');
  assert.equal(loaded.decisions, null);
  assert.match(loaded.decisionsError, /RSI daemon is unavailable/);
  assert.equal(loaded.events.length, 1);
  assert.equal(loaded.session.id, 's-1');
});

test('a 401 on decisions re-bootstraps once and retries', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [summary()] },
    detail: { 's-1': detail(summary()) },
    history: { 's-1': [] },
    decisions: { 's-1': [genericDecision()] },
    decisions401Once: 1,
  });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const loaded = await api.getSession('s-1');
  assert.equal(loaded.decisionsError, '');
  assert.equal(loaded.decisions.length, 1);
  assert.equal(gw.paths().filter((path) => path === '/auth/bootstrap').length, 2);
  assert.equal(gw.paths().filter((path) => path.endsWith('/decisions')).length, 2);
});

const sessionPage = (items, token) => ({ items, next_cursor: token === undefined ? null : { kind: 'sessions', token } });

test('listSessions follows a sessions cursor to the final page', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessionPages: { 'p-1': (cursor) => (cursor === null
      ? sessionPage([summary({ id: 's-1' })], 'tok2')
      : sessionPage([summary({ id: 's-2' })], undefined)) },
  });
  const api = createApi(gw.fetchImpl);
  const sessions = await api.listSessions();
  assert.deepEqual(sessions.map((session) => session.id), ['s-1', 's-2']);
  assert.deepEqual(gw.paths(), [
    '/auth/bootstrap', '/auth/session', '/api/v1/projects',
    '/api/v1/projects/p-1/sessions',
    '/api/v1/projects/p-1/sessions?cursor=tok2',
  ]);
});

test('listSessions stops after four session pages per project', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessionPages: { 'p-1': (cursor) => {
      const index = cursor === null ? 0 : Number(cursor.slice(3));
      return sessionPage([summary({ id: `s-${index}` })], `tok${index + 1}`);
    } },
  });
  const api = createApi(gw.fetchImpl);
  const sessions = await api.listSessions();
  assert.deepEqual(sessions.map((session) => session.id), ['s-0', 's-1', 's-2', 's-3']);
  assert.equal(gw.paths().filter((path) => path.startsWith('/api/v1/projects/p-1/sessions')).length, 4);
});

test('a wrong-kind, unsafe or malformed session cursor fails', async () => {
  const cursorGateway = (next_cursor) => makeGateway({ projects: [project('p-1')], sessionPages: { 'p-1': () => ({ items: [summary()], next_cursor }) } });
  await assert.rejects(createApi(cursorGateway({ kind: 'projects', token: 'tok' }).fetchImpl).listSessions(), /cursor/);
  await assert.rejects(createApi(cursorGateway({ kind: 'sessions', token: 'not a token!' }).fetchImpl).listSessions(), /cursor/);
  await assert.rejects(createApi(cursorGateway({ kind: 'sessions' }).fetchImpl).listSessions(), /cursor/);
  await assert.rejects(createApi(cursorGateway('tok').fetchImpl).listSessions(), /cursor/);
});

test('duplicate identities across cursor pages fail', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessionPages: { 'p-1': (cursor) => (cursor === null
      ? sessionPage([summary({ id: 's-1' })], 'tok2')
      : sessionPage([summary({ id: 's-1' })], undefined)) },
  });
  await assert.rejects(createApi(gw.fetchImpl).listSessions(), /duplicate identity/);
});

test('getSession marks hasOlder when the latest page is full and keeps sequence/id', async () => {
  const full = Array.from({ length: 25 }, (_value, index) => historyEvent({ id: String(index + 1), sequence: index + 1 }));
  const gw = makeGateway({ projects: [project('p-1')], sessions: { 'p-1': [summary()] }, detail: { 's-1': detail(summary()) }, history: { 's-1': full } });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const loaded = await api.getSession('s-1');
  assert.equal(loaded.hasOlder, true);
  assert.equal(loaded.events.length, 25);
  assert.deepEqual([loaded.events[0].id, loaded.events[0].sequence], ['1', 1]);

  const small = createApi(makeGateway({ projects: [project('p-1')], sessions: { 'p-1': [summary()] }, detail: { 's-1': detail(summary()) }, history: { 's-1': [historyEvent()] } }).fetchImpl);
  await small.listSessions();
  assert.equal((await small.getSession('s-1')).hasOlder, false);
});

test('getOlderHistory anchors an older page through the remembered project', async () => {
  const older = [historyEvent({ id: '40', sequence: 4, kind: KNOWN('Message'), role: KNOWN('User') }), historyEvent({ id: '41', sequence: 5 })];
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [summary()] },
    detail: { 's-1': detail(summary()) },
    history: { 's-1': [historyEvent({ id: '42', sequence: 6 })] },
    olderHistory: { 's-1': { '6:42': { items: older } } },
  });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const page = await api.getOlderHistory('s-1', { sequence: 6, id: '42' });
  assert.deepEqual(page.events.map((event) => [event.id, event.sequence, event.kind]), [['40', 4, 'user'], ['41', 5, 'assistant']]);
  assert.equal(page.hasOlder, false);
  assert.ok(gw.paths().includes('/api/v1/projects/p-1/sessions/s-1/history?before=6:42'));
});

test('getOlderHistory validates the anchor before any request', async () => {
  const gw = makeGateway({ projects: [project('p-1')], sessions: { 'p-1': [summary()] }, detail: { 's-1': detail(summary()) }, history: { 's-1': [] } });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const calls = gw.calls.length;
  await assert.rejects(api.getOlderHistory('s-1', { sequence: 1, id: '0' }), /Invalid history anchor/);
  await assert.rejects(api.getOlderHistory('s-1', { sequence: 1, id: '01' }), /Invalid history anchor/);
  await assert.rejects(api.getOlderHistory('s-1', { sequence: 1.5, id: '4' }), /Invalid history anchor/);
  await assert.rejects(api.getOlderHistory('s-1', { sequence: '1', id: '4' }), /Invalid history anchor/);
  await assert.rejects(api.getOlderHistory('s-1', null), /Invalid history anchor/);
  assert.equal(gw.calls.length, calls);
  await assert.rejects(api.getOlderHistory('s-9', { sequence: 1, id: '4' }), /Refresh the session list/);
});

test('a 401 on the older history call re-bootstraps once and retries', async () => {
  const gw = makeGateway({
    projects: [project('p-1')],
    sessions: { 'p-1': [summary()] },
    detail: { 's-1': detail(summary()) },
    history: { 's-1': [] },
    olderHistory: { 's-1': { '3:9': { items: [historyEvent({ id: '8', sequence: 2 })] } } },
    olderHistory401Once: 1,
  });
  const api = createApi(gw.fetchImpl);
  await api.listSessions();
  const page = await api.getOlderHistory('s-1', { sequence: 3, id: '9' });
  assert.equal(page.events.length, 1);
  assert.equal(page.events[0].id, '8');
  assert.equal(gw.paths().filter((path) => path === '/auth/bootstrap').length, 2);
  assert.equal(gw.paths().filter((path) => path.includes('/history?before=')).length, 2);
});
