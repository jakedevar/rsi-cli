// Archive when finished and the pane after Archive (local #308, #309), through the real header and list drawing:
// a Running session shows a pending marker in the header and on its row, the marker clears, and a session that leaves the
// Active list between two polls (archived from the TUI or by an agent) is drawn as what rsid says it is, read-only.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const permissive = (target) => new Proxy(target, {
  get(t, k) { if (k in t) return t[k]; if (typeof k === 'symbol') return undefined; return (...a) => (k === 'closest' || k === 'querySelector' ? null : k === 'querySelectorAll' ? [] : undefined); },
});
class Node {
  constructor(tag) {
    this.tagName = String(tag).toUpperCase(); this.children = []; this.dataset = {}; this.attrs = {}; this.listeners = {}; this.parent = null;
    this.className = ''; this.disabled = false; this._text = ''; this.tabIndex = -1; this.hidden = false; this.title = ''; this.id = ''; this.placeholder = ''; this.value = '';
    this.style = { setProperty() {} };
    const classes = new Set();
    this.classList = { add: (c) => classes.add(c), remove: (c) => classes.delete(c), toggle: (c, on) => { if (on === undefined ? !classes.has(c) : on) classes.add(c); else classes.delete(c); }, contains: (c) => classes.has(c) };
    return permissive(this);
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  replaceChildren(...n) { this._text = ''; this.children = []; this.append(...n); }
  append(...n) { for (const c of n) { c.parent = this; this.children.push(c); } }
  prepend(...n) { for (const c of n) c.parent = this; this.children.unshift(...n); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  removeAttribute(k) { delete this.attrs[k]; }
  addEventListener(t, cb) { (this.listeners[t] ??= []).push(cb); }
  focus() { globalThis.document.activeElement = this; }
  contains(n) { return n === this || this.children.some((c) => c.contains?.(n)); }
  get firstChild() { return this.children[0] ?? null; }
  get nextSibling() { return this.parent ? this.parent.children[this.parent.children.indexOf(this) + 1] ?? null : null; }
  removeChild(c) { this.children = this.children.filter((x) => x !== c); c.parent = null; return c; }
  insertBefore(node, ref) { if (node.parent) node.parent.children = node.parent.children.filter((x) => x !== node); const at = ref ? this.children.indexOf(ref) : this.children.length; this.children.splice(at, 0, node); node.parent = this; return node; }
  all(pred, out = []) { for (const c of this.children) { if (pred(c)) out.push(c); c.all(pred, out); } return out; }
}

const byId = new Map();
const nodeFor = (id) => { let n = byId.get(id); if (!n) { n = new Node('div'); n.id = id; byId.set(id, n); } return n; };
const body = new Node('body');
globalThis.document = {
  documentElement: new Node('html'), body, activeElement: body,
  createElement: (t) => new Node(t),
  getElementById: (id) => nodeFor(id),
  querySelectorAll: () => [],
  querySelector: () => null,
  addEventListener() {},
};
globalThis.window = { addEventListener() {}, setTimeout, clearTimeout };
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };

const ID = '11111111-aaaa-4bbb-8ccc-000000000001';
const base = { provider: 'Claude', working_dir: '/w', query: 'q', created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T12:00:00Z' };
const mk = (over = {}) => ({ ...base, id: ID, status: 'Running', title: 'Fake worker', ...over });
const remote = { list: [], sessions: {}, calls: [] };
globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => {
  remote.calls.push([cmd, args]);
  if (cmd === 'list_sessions') return remote.list;
  if (cmd === 'list_archived_sessions') return [];
  if (cmd === 'get_session') { const s = remote.sessions[args.sessionId]; if (!s) throw new Error('Session not found'); return s; }
  if (cmd === 'get_operator_pause') return { pause_level: 'none' };
  if (cmd === 'get_conversation') return [];
  return null;
} } };

await import('../dist/sessions.js');
const { refreshSessions, renderHeader, renderSessionList, select } = await import('../dist/sessions.js');
const { state } = await import('../dist/state.js');
const { initApprovals, archive } = await import('../dist/approvals.js');
const { toast } = await import('../dist/dom.js');
initApprovals({ toast() {}, refresh() {}, selected: () => state.sessions.find((s) => s.id === state.selectedId), redraw() {} });

const settle = () => new Promise((r) => setTimeout(r, 5));
const text = (id) => nodeFor(id).textContent;
const rowText = () => nodeFor('session-list').children.map((li) => li.textContent).join('|');

test('a running session marked to archive shows the marker in the header and on its row, and the button offers to cancel', async () => {
  state.view = 'active';
  state.sessions = [mk({ pending_archive: true })];
  state.selectedId = ID;
  renderSessionList();
  renderHeader();
  assert.match(text('s-meta'), /Archives when finished/);
  assert.match(rowText(), /archives when finished/);
  assert.equal(nodeFor('archive').textContent, 'Cancel auto-archive');
  assert.match(nodeFor('session-list').children[0].getAttribute('aria-label'), /archives when finished/);
  // Marker gone when rsid clears it, and never shown on a session that is no longer running.
  state.sessions = [mk({ pending_archive: false })];
  renderSessionList(); renderHeader();
  assert.doesNotMatch(text('s-meta'), /Archives when finished/);
  assert.equal(nodeFor('archive').textContent, 'Archive when finished');
  state.sessions = [mk({ status: 'Completed', pending_archive: true })];
  renderSessionList(); renderHeader();
  assert.doesNotMatch(text('s-meta'), /Archives when finished/);
  assert.equal(nodeFor('archive').textContent, 'Archive');
});

test('a WaitingApproval session is live too: it shows Archive when finished, and the marker once marked (local #315)', () => {
  state.view = 'active';
  state.selectedId = ID;
  state.sessions = [mk({ status: 'WaitingApproval' })];
  renderSessionList(); renderHeader();
  assert.equal(nodeFor('archive').textContent, 'Archive when finished');
  assert.doesNotMatch(text('s-meta'), /Archives when finished/);
  state.sessions = [mk({ status: 'WaitingApproval', pending_archive: true })];
  renderSessionList(); renderHeader();
  assert.equal(nodeFor('archive').textContent, 'Cancel auto-archive');
  assert.match(text('s-meta'), /Archives when finished/);
  assert.match(rowText(), /archives when finished/);
});

test('the open session leaves the Active list between two polls because it was archived: the pane shows Archived, read-only, with Restore', async () => {
  state.view = 'active';
  state.departed = null;
  state.selectedId = ID;
  remote.list = [mk({ status: 'Completed' })];
  assert.equal(await refreshSessions(), null);
  assert.equal(text('s-meta').includes('Completed'), true);
  assert.equal(nodeFor('message').disabled, false);
  assert.equal(nodeFor('archive').hidden, false);
  // Next poll: archived elsewhere (the TUI, an agent). rsid no longer lists it as active but still answers GetSession.
  remote.list = [];
  remote.sessions[ID] = mk({ status: 'Archived' });
  assert.equal(await refreshSessions(), null);
  assert.equal(state.sessions.length, 0);
  assert.match(text('s-meta'), /Archived/);
  assert.equal(nodeFor('message').disabled, true, 'no message box');
  assert.equal(nodeFor('message').placeholder, 'Archived: Restore it to message it');
  assert.equal(nodeFor('archive').hidden, true, 'no Archive');
  assert.equal(nodeFor('restore').hidden, false, 'Restore');
  assert.equal(nodeFor('interrupt').disabled, true, 'no Stop');
  // It comes back (restored in the TUI): the pane is the live session again.
  remote.list = [mk({ status: 'Completed' })];
  assert.equal(await refreshSessions(), null);
  assert.equal(state.departed, null);
  assert.equal(nodeFor('restore').hidden, true);
  assert.equal(nodeFor('message').disabled, false);
});

test('a session that is simply not in the list yet (just launched) is left alone, and a failed lookup keeps the pane', async () => {
  state.departed = null;
  state.selectedId = ID;
  remote.list = [];
  remote.sessions[ID] = mk({ status: 'Running' });
  assert.equal(await refreshSessions(), null);
  assert.equal(state.departed, null, 'only an archived or deleted answer counts as having left');
  delete remote.sessions[ID];
  assert.equal(await refreshSessions(), null);
  assert.equal(state.departed, null);
});

test('a deleted session that left the list is read-only and says it is no longer in the list', async () => {
  state.departed = null;
  state.selectedId = ID;
  remote.list = [];
  remote.sessions[ID] = mk({ status: 'Deleted' });
  const failure = await refreshSessions();
  assert.equal(failure, null);
  assert.equal(state.departed?.status, 'Deleted');
  assert.equal(nodeFor('message').disabled, true);
  assert.equal(nodeFor('message').placeholder, 'This session is no longer in the list');
  assert.equal(nodeFor('archive').disabled, true);
  // Opening another session drops the departed one.
  remote.list = [mk({ id: '22222222-aaaa-4bbb-8ccc-000000000002', title: 'Other', status: 'Completed' })];
  select('22222222-aaaa-4bbb-8ccc-000000000002');
  assert.equal(state.departed, null);
  await settle();
});

test('Archive from the header, through a daemon that drops the session from ListSessions: the pane ends as Archived with Restore, and Undo brings the live session back (local #309)', async () => {
  // A daemon that behaves as rsid does: archive_session moves the session out of the Active list; unarchive_session moves it back.
  const live = mk({ status: 'Completed' });
  remote.list = [live];
  remote.sessions[ID] = live;
  const was = globalThis.__TAURI__.core.invoke;
  globalThis.__TAURI__.core.invoke = async (cmd, args) => {
    if (cmd === 'archive_session') { remote.list = []; remote.sessions[ID] = { ...live, status: 'Archived' }; remote.calls.push([cmd, args]); return null; }
    if (cmd === 'unarchive_session') { remote.list = [live]; remote.sessions[ID] = live; remote.calls.push([cmd, args]); return null; }
    return was(cmd, args);
  };
  // The confirmation answers yes the way a press would, and the header redraw and the refresh are the app's own.
  const realCreate = document.createElement;
  document.createElement = (t) => {
    const n = realCreate(t);
    if (t === 'dialog') n.showModal = () => queueMicrotask(() => { n.children[0].listeners.submit?.forEach((cb) => cb({})); n.listeners.close?.forEach((cb) => cb({})); });
    return n;
  };
  initApprovals({ toast, refresh: () => { void refreshSessions(); }, selected: () => state.sessions.find((s) => s.id === state.selectedId) ?? (state.departed?.id === state.selectedId ? state.departed : undefined), redraw: renderHeader });
  try {
    state.view = 'active';
    state.departed = null;
    state.selectedId = ID;
    assert.equal(await refreshSessions(), null);
    assert.equal(nodeFor('archive').hidden, false);
    assert.equal(nodeFor('message').disabled, false);

    await archive();
    await settle();
    assert.equal(state.sessions.length, 0, 'the list no longer has it');
    assert.match(text('s-meta'), /Archived/);
    assert.doesNotMatch(text('s-meta'), /Completed/);
    assert.equal(nodeFor('archive').hidden, true, 'no Archive');
    assert.equal(nodeFor('restore').hidden, false, 'Restore');
    assert.equal(nodeFor('message').disabled, true, 'no message box');
    assert.equal(nodeFor('message').placeholder, 'Archived: Restore it to message it');
    assert.equal(nodeFor('interrupt').disabled, true, 'no Stop');
    assert.match(text('toast'), /Archived/);

    // Undo on the toast restores the pane.
    const undo = nodeFor('toast').all((n) => n.tagName === 'BUTTON')[0];
    assert.ok(undo, 'the toast has Undo');
    undo.listeners.click.forEach((cb) => cb({}));
    await settle();
    assert.equal(state.departed, null);
    assert.match(text('s-meta'), /Completed/);
    assert.equal(nodeFor('archive').hidden, false);
    assert.equal(nodeFor('restore').hidden, true);
    assert.equal(nodeFor('message').disabled, false);
    assert.deepEqual(remote.calls.filter(([c]) => c === 'archive_session' || c === 'unarchive_session').map(([c]) => c), ['archive_session', 'unarchive_session']);
  } finally {
    document.createElement = realCreate;
    globalThis.__TAURI__.core.invoke = was;
    initApprovals({ toast() {}, refresh() {}, selected: () => state.sessions.find((s) => s.id === state.selectedId), redraw() {} });
  }
});
