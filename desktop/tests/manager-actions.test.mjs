// Manager views with Vim keys off (local #223, from the #221 audit): every VIEW_KEYS action
// that is not a standard key has a button in the view's action bar, the button is enabled
// when the action applies and runs the same action (same RPC) on click, and the confirm
// dialogs say what happens in plain words with the RPC under Details. The DOM side runs
// against a minimal fake document (no browser needed).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { STANDARD_KEYS } from '../dist/modal-gate.js';
import { VIEW_KEYS, defaultPolicy, POLICY_FIELDS } from '../dist/managers-model.js';
import { formatKeyHint, offText, stripKeyLabel } from '../dist/key-hints.js';
import { gettingStartedRows } from '../dist/keyhelp-model.js';
import {
  BOARD_BARS, MANAGER_EXPLAINER, NAV_ACTIONS, NO_PORTFOLIO_NODES, VIEW_BARS, appointProjectCopy, barLabel, clearScopeCopy,
  revokeGlobalCopy, revokeProjectCopy,
} from '../dist/manager-actions-model.js';

const P = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';
const S = '0b7c8a52-5f63-4a9e-9d27-3c1a9a5b6e10';
const S2 = '1c8d9b63-6a74-4b5f-8e38-4d2b0b6c7f21';
const CAND = '4c5d6e7f-8a9b-4c0d-9e1f-2a3b4c5d6e7f';
const E1 = '2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d';
const RPC = /\b(?:Configure|Revoke|Get|List|Answer|Archive)[A-Z]\w+|\bRPC\b|\bv\d/;

// ---------- the bars cover the keys (pure) ----------

const barsOf = (view) => (view === 'board' ? Object.values(BOARD_BARS).flat() : VIEW_BARS[view]);

test('every view key that is not a standard key or navigation has a button in that view’s bar', () => {
  for (const [view, keys] of Object.entries(VIEW_KEYS)) {
    const offered = new Set(barsOf(view).map((b) => b.action));
    for (const [key, action] of Object.entries(keys)) {
      if (STANDARD_KEYS.has(key) || NAV_ACTIONS.has(action)) continue;
      assert.ok(offered.has(action), `${view}: ${key} (${action}) has a button`);
    }
  }
});

test('each button names the key that runs the same action, and drops it with Vim keys off', () => {
  for (const view of Object.keys(VIEW_KEYS)) {
    for (const b of barsOf(view)) {
      assert.equal(VIEW_KEYS[view][b.key], b.action, `${view}: ${b.label} key ${b.key}`);
      const label = barLabel(b);
      assert.match(label, /\(.+\)$/, `${label} shows its key`);
      const off = offText('button', label);
      if (STANDARD_KEYS.has(b.key)) assert.equal(off, label, `${label} keeps a key that works with Vim keys off`);
      else assert.equal(off, b.label, `${label} drops a Vim-only key`);
    }
  }
  assert.equal(stripKeyLabel('Raise one step (+)'), 'Raise one step');
  assert.equal(stripKeyLabel('Settings (all)'), 'Settings (all)');
});

test('the issue’s minimum set of actions is on the bars', () => {
  const labels = new Set([...Object.values(BOARD_BARS).flat(), ...Object.values(VIEW_BARS).flat()].map((b) => b.label));
  for (const want of ['Save policy', 'Discard changes', 'Answer…', 'Accept recommendation', 'Archive stale…', 'Open session',
    'Appoint', 'Appoint above', 'Edit', 'Revoke', 'Move', 'Whole project', 'Filter…', 'Type to this seat…', 'Talk to the seat…',
    'Halt', 'Interrupt now…', 'Launch and appoint…', 'Raise one step', 'Sort…', 'Group', 'Window']) {
    assert.ok(labels.has(want), want);
  }
});

test('the hint line drops "/ filter" with Vim keys off', () => {
  assert.equal(formatKeyHint('j/k · / filter · Tab agents/usage · Enter open · Esc'), 'F6 agents/usage · Enter open · Esc');
  assert.equal(formatKeyHint('j/k · Space toggle · a whole project · / filter · Enter save · Esc cancel'), 'Enter save · Esc cancel');
});

// ---------- confirm copy (pure) ----------

test('confirm dialogs say what happens in plain words; the RPC and versions go under Details', () => {
  const copies = [
    revokeProjectCopy('rsi', '2 row(s) below it are affected.', 4),
    clearScopeCopy('rsi', 4),
    revokeGlobalCopy(3, S, '5 row(s) below it are affected.'),
    revokeGlobalCopy(3, S),
    appointProjectCopy('rsi', S, CAND, true, 4),
    appointProjectCopy('rsi', null, CAND, false, 0),
  ];
  for (const c of copies) {
    for (const line of [c.title, ...c.lines]) assert.doesNotMatch(line, RPC, line);
    assert.match(c.details, /^(?:ConfigureHarnessManager|RevokeGlobalManager)\b/);
    assert.match(c.details, /v\d/);
  }
  assert.equal(revokeProjectCopy('rsi', 'x', 4).lines[0], 'Stop this manager from supervising rsi. Its session keeps running but loses its authority.');
  assert.equal(clearScopeCopy('rsi', 4).lines[0], 'Stop this manager from supervising rsi. Its session keeps running but loses its authority.');
  assert.equal(appointProjectCopy('rsi', S, CAND, true, 4).lines[0], 'The selected session 4c5d6e7f becomes the manager of rsi, replacing 0b7c8a52.');
  assert.match(NO_PORTFOLIO_NODES, /^No portfolio nodes yet\. A portfolio node is a manager that looks after several projects/);
});

test('"What is a manager?" is the Getting started row', () => {
  const row = gettingStartedRows('linux').find((r) => r.title === 'Manager');
  assert.equal(row.detail, MANAGER_EXPLAINER);
});

// ---------- fake DOM ----------

function matchesSimple(n, sel) {
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)((?:\[[^\]]+\])*)$/i.exec(sel);
  if (!m) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  for (const c of m[2].split('.').filter(Boolean)) if (!n.className.split(/\s+/).includes(c)) return false;
  for (const a of m[3].match(/\[[^\]]+\]/g) ?? []) {
    const [, name, val] = /^\[([\w-]+)(?:="([^"]*)")?\]$/.exec(a) ?? [];
    if (name === 'open') { if (!n.open) return false; continue; }
    if (!(name in n.attrs) || (val !== undefined && n.attrs[name] !== val)) return false;
  }
  return true;
}
function matches(n, selector) {
  return selector.split(',').map((s) => s.trim()).some((sel) => {
    const parts = sel.split(/\s+/);
    if (!matchesSimple(n, parts.at(-1))) return false;
    let at = n.parent;
    for (let i = parts.length - 2; i >= 0; i--) {
      while (at && !matchesSimple(at, parts[i])) at = at.parent;
      if (!at) return false;
      at = at.parent;
    }
    return true;
  });
}

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.listeners = {}; this._text = ''; this.className = ''; this.attrs = {}; this.parent = null;
    this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this.value = ''; this.title = ''; this.clientHeight = 400;
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...kids) { for (const k of kids) { const n = typeof k === 'string' ? Object.assign(new FakeEl('#text'), { _text: k }) : k; n.parent = this; this.children.push(n); } }
  replaceChildren(...kids) { this._text = ''; this.children = []; this.append(...kids); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  removeAttribute(k) { delete this.attrs[k]; }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matches(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  fire(t, ev = {}) { const e = { target: this, preventDefault() { e.prevented = true; }, ...ev }; for (const f of this.listeners[t] ?? []) f(e); return e; }
  /** A click bubbles to the ancestors' listeners. */
  click() { const e = { target: this, preventDefault() {}, stopPropagation() {} }; for (let n = this; n; n = n.parent) for (const f of n.listeners.click ?? []) f(e); }
  closest(sel) { for (let n = this; n; n = n.parent) if (n.tag !== '#text' && matches(n, sel)) return n; return null; }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this); this.parent = null; }
  focus() { doc.activeElement = this; }
  scrollIntoView() {}
  scrollBy() {}
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  showModal() { this.open = true; }
  close() { this.open = false; }
}

const body = new FakeEl('body');
const toastEl = new FakeEl('div');
const doc = {
  documentElement: { dataset: {} }, title: '', activeElement: null,
  addEventListener() {},
  head: new FakeEl('head'), body,
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => (id === 'toast' ? toastEl : new FakeEl('div')),
  createElement: (tag) => new FakeEl(tag),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};
const keyListeners = [];
const realSetInterval = globalThis.setInterval;
const realSetTimeout = globalThis.setTimeout;
globalThis.window = globalThis;
globalThis.setInterval = (...a) => realSetInterval(...a).unref();
globalThis.setTimeout = (...a) => { const t = realSetTimeout(...a); t?.unref?.(); return t; };
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };
globalThis.addEventListener = (type, f) => { if (type === 'keydown') keyListeners.push(f); };
globalThis.document = doc;
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};
const copied = [];
Object.defineProperty(globalThis.navigator, 'clipboard', { value: { writeText: async (t) => { copied.push(t); } }, configurable: true });

const managers = await import('../dist/managers.js');
const { state } = await import('../dist/state.js');
const { onToast } = await import('../dist/dom.js');
const toasts = [];
onToast((msg) => toasts.push(msg));
managers.initManagers();

// ---------- a scripted daemon ----------

const load = { running_workers: 1, direct_reports: null, pending_escalations: 0, pending_decisions: 0 };
const treeRow = (key, parent, depth, kind, over = {}) => ({
  key, parent_key: parent, depth, kind, label: key, project_id: null, node_id: null, epic_id: null, scope: null,
  seat: null, focus_session_id: null, grant: null, load, complete: true, ...over,
});
const grant = { grant_version: 3, capabilities: [], max_active_sessions: 1, max_created_sessions: 1, max_created_containers: 1, max_direct_reports: 1, max_spend_usd: null, reserved: [] };
const TREE = [
  treeRow('global', null, 0, 'global', { label: 'Global manager', grant, focus_session_id: S2 }),
  treeRow(`project:${P}`, 'global', 1, 'project', { label: 'rsi', project_id: P, focus_session_id: S }),
];
const config = { project_id: P, manager_session_id: S, epic_ids: [], scope_mode: 'project', row_version: 4, updated_at: '' };
const savedPolicy = { project_id: P, manager_session_id: S, scope_version: 4, row_version: 9, policy: defaultPolicy(), updated_at: '', revoked: false };
const decision = { key: 'd:1', status: 'pending', row_version: 3, target_digest: 'abc', question: 'Ship it?\nA) Yes\nB) No (recommended)' };
const statePage = (section, rows) => ({ observed_at: 't', scope_version: 4, policy: savedPolicy, section, rows, next_cursor: null, complete: true });
const seat = { session_id: S, project_id: P, status: 'Running', provider: 'Claude', model: 'opus', context_fill_pct: 50, cost_usd: 1, updated_at: '' };
const workspace = {
  grant: { grant_id: E1, grant_version: 3, seat_session_id: S, state: 'active', project_ids: [P], project_policy: {}, operator_origin: 'tui', updated_at: '' },
  seat,
  projects: [{ scope_revoked: false, overview: {
    project_id: P, name: 'rsi', path: null, manager: null, policy: null,
    issues: { open: 0, in_progress: 0, open_operator_requests: 0 }, running_sessions: 0, waiting_approval_sessions: 0, pending_questions: 0, pending_approvals: 0,
  } }],
  missing_project_ids: [],
};
const budget = {
  node_id: S2, node_label: 'global', resets: 'no timed reset',
  projects: [{ project_id: P, project_name: 'rsi', chain: [
    { kind: 'portfolio', node_id: S2, label: 'global', version: 4, epoch: 2, counted_since: null, caps: [{ field: 'created_sessions', used: 30, limit: 32, binding: true }] },
    { kind: 'delegate', node_id: E1, label: 'area', version: 1, epoch: 1, counted_since: null, caps: [{ field: 'active_sessions', used: 1, limit: 2 }] },
  ] }],
};
const usage = (cost) => ({ invocations: 10, errors: 1, input: 600, output: 0, cache_read: 0, cache_write: 0, cost, unknown_usage: 0 });
const fleet = {
  as_of: 't',
  agents: [{ session: { id: S, provider: 'Claude', model: 'm', status: 'Running', title: 't', context_fill_pct: 10 }, project: 'rsi', role: 'worker', turn_started_at: null }],
  groups: [{ dimension: 'project', key: P, label: 'rsi', active: 1, windows: [usage(1), usage(2), usage(3)] }],
  totals: [usage(1), usage(2), usage(3)], agents_truncated: false, usage_truncated: false,
};

let calls = [];
function daemon(over = {}) {
  calls = [];
  globalThis.__TAURI__ = {
    core: {
      invoke: async (cmd, args) => {
        calls.push({ cmd, args });
        if (cmd in over) return typeof over[cmd] === 'function' ? over[cmd](args) : over[cmd];
        switch (cmd) {
          case 'manager_tree': return { rows: TREE, next_after: null, total_rows: TREE.length, complete: true, global_grant_version: 3 };
          case 'harness_manager_get': return config;
          case 'harness_manager_policy_get': return savedPolicy;
          case 'mgr_policy_classify': return { preset: 'custom', profile: null, suggestions: [], root_group_mismatch: false, validation_error: null };
          case 'harness_manager_state': return statePage(args.section, args.section === 'decisions' ? [decision] : []);
          case 'harness_manager_configure_policy': return { ...savedPolicy, row_version: 10, policy: args.request.policy };
          case 'harness_manager_configure_scope': return { row_version: 5 };
          case 'harness_manager_scope_page': return { rows: [{ id: E1, title: 'Epic one', kind: 'Epic', group_id: null, group_title: null }], next_after_id: null };
          case 'harness_manager_answer_decision': return {};
          case 'manager_stale_decisions': return { older_than_days: 7, rows: [{ decision: { key: 'd:old' }, question: 'Old?', status: 'pending' }], complete: true };
          case 'manager_archive_stale_decisions': return {};
          case 'global_manager_workspace': return workspace;
          case 'get_conversation': return [];
          case 'get_session': return { id: args.sessionId, status: 'Running' };
          case 'pause_session': case 'send_message': return {};
          case 'global_manager_budget': return budget;
          case 'fleet_overview': return fleet;
          case 'portfolio_nodes_list': return { nodes: [] };
          default: return null;
        }
      },
    },
  };
}

const tick = async (n = 6) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };
const view = () => body.querySelector('.mgr-view');
const bar = () => view().querySelector('.mgr-actions');
/** The bar button whose label (without the key) is `label`. */
const btn = (label) => {
  const b = bar().querySelectorAll('button').find((x) => x.textContent.replace(/ \([^()]+\)$/, '') === label);
  assert.ok(b, `a ${label} button (bar: ${bar().querySelectorAll('button').map((x) => x.textContent).join(', ')})`);
  return b;
};
const enabled = (label) => { const b = btn(label); assert.equal(b.disabled, false, `${label} is enabled (${b.title})`); return b; };
const disabled = (label) => { const b = btn(label); assert.equal(b.disabled, true, `${label} is disabled`); assert.ok(b.title, `${label} says why`); return b; };
const selectRow = async (i) => { view().querySelectorAll('.mgr-item')[i].click(); await tick(); };
const openDialog = () => body.querySelectorAll('dialog').filter((d) => d.open).at(-1);
async function confirm(input) {
  const d = openDialog();
  assert.ok(d, 'a dialog is open');
  const field = d.querySelector('textarea') ?? d.querySelector('input');
  if (input !== undefined) field.value = input;
  const ok = d.querySelectorAll('button').find((b) => b.type === 'submit');
  d.querySelector('form').fire('submit', { submitter: ok });
  await tick();
}
const dialogText = (d) => d.querySelectorAll('h2, p').map((n) => n.textContent);
const called = (cmd) => calls.filter((c) => c.cmd === cmd);

state.projects = [{ id: P, name: 'rsi' }];
state.sessions = [{ id: CAND, project_id: P, status: 'Running', session_kind: 'Standard', title: 'cand' }];
state.selectedId = CAND;

// ---------- each view, by click ----------

test('tree: Appoint and Revoke are buttons that reach the same requests; confirms speak plainly', async () => {
  daemon();
  managers.runManagers('mgr-tree', {});
  await tick();
  assert.equal(view().querySelector('.mgr-about').hidden, false, 'the tree explains what a manager is');
  assert.match(view().querySelector('.mgr-about').textContent, /What is a manager\?A session appointed/);
  await selectRow(1);
  for (const l of ['Open', 'Appoint', 'Edit', 'Revoke', 'Preview', 'Open project']) enabled(l);
  disabled('Appoint above');
  disabled('Move');
  disabled('Load more');
  btn('Revoke').click();
  await tick();
  let d = openDialog();
  const text = dialogText(d);
  assert.equal(text[0], 'Stop the manager of rsi?');
  assert.ok(text.includes('Stop this manager from supervising rsi. Its session keeps running but loses its authority.'));
  for (const t of text) assert.doesNotMatch(t, RPC, t);
  assert.match(d.querySelector('details').textContent, /ConfigureHarnessManager with an empty selection \(expects scope v4\)/);
  await confirm();
  const revoke = called('harness_manager_configure_scope').at(-1);
  assert.ok(revoke, 'revoke sent');
  assert.equal(revoke.args.request.expected_row_version, 4);

  daemon();
  await selectRow(1);
  btn('Appoint').click();
  await tick();
  d = openDialog();
  assert.equal(dialogText(d)[0], 'Replace the manager of rsi?');
  assert.ok(d.querySelector('details'));
  await confirm();
  assert.equal(called('harness_manager_configure_scope').at(-1)?.args.request.session_id, CAND);
});

test('board decisions: Answer, Accept recommendation and Archive stale by click', async () => {
  daemon();
  managers.runManagers('mgr-decisions', {});
  await tick();
  assert.equal(view().querySelector('.mgr-about').hidden, false, 'the board explains what a manager is');
  for (const l of ['Send highlighted option', 'Answer…', 'Accept recommendation', 'Archive stale…']) enabled(l);
  disabled('Open session');
  disabled('Next page');
  disabled('Previous page');
  btn('Accept recommendation').click();
  await tick();
  assert.match(called('harness_manager_answer_decision').at(-1).args.request.answer, /^B/);

  daemon();
  btn('Answer…').click();
  await tick();
  await confirm('Not yet');
  assert.equal(called('harness_manager_answer_decision').at(-1).args.request.answer, 'Not yet');

  daemon();
  btn('Archive stale…').click();
  await tick();
  await confirm();
  assert.equal(called('manager_archive_stale_decisions').length, 1);
});

test('board policy: Edit field, then Save policy (and Discard changes) by click', async () => {
  daemon();
  managers.runManagers('mgr-policy', {});
  await tick();
  disabled('Save policy');
  disabled('Discard changes');
  const at = POLICY_FIELDS.findIndex((f) => f.kind === 'cycle' || f.kind === 'bool');
  await selectRow(at);
  enabled('Edit field').click();
  await tick();
  assert.match(view().querySelector('.mgr-meta').textContent, /unsaved changes: Save policy keeps them, Discard changes drops them/);
  assert.doesNotMatch(view().querySelector('.mgr-meta').textContent, /scope v\d|\bs saves\b/);
  enabled('Discard changes');
  enabled('Save policy').click();
  await tick();
  const save = called('harness_manager_configure_policy').at(-1);
  assert.ok(save, 'the policy is saved');
  assert.notDeepEqual(save.args.request.policy, defaultPolicy());
  disabled('Save policy');
});

test('scope: Toggle, Whole project, Filter and Save by click', async () => {
  daemon();
  managers.runManagers('mgr-scope', {});
  await tick();
  disabled('Whole project');
  enabled('Toggle').click();
  await tick();
  enabled('Whole project').click();
  await tick();
  disabled('Whole project');
  enabled('Filter…').click();
  await tick();
  await confirm('Epic');
  assert.match(view().querySelector('.mgr-meta').textContent, /filter “Epic”/);
  assert.match(view().querySelector('.mgr-meta').textContent, /^manager session 0b7c8a52 · .* Group\(s\) and Epic\(s\) to choose from/);
  enabled('Save').click();
  await tick();
  assert.equal(called('harness_manager_configure_scope').length, 1);
  assert.match(view().querySelector('.mgr-detail .mgr-note').textContent, /Click Toggle \(or press Space\) to choose a row, click Whole project \(or press a\) to cover everything, then click Save \(or press Enter\)\./);
  assert.equal(view().querySelector('.mgr-detail .mgr-note').dataset.keyTip, '', 'the key-hints pass drops the Vim keys');
});

test('workspace: Talk, Halt, Interrupt now and Launch by click; Archive and Up a level say why not', async () => {
  daemon();
  managers.runManagers('mgr-global', {});
  await tick();
  for (const l of ['Open', 'Type to this seat…', 'Talk to the seat…', 'Status', 'Halt', 'Interrupt now…', 'Launch and appoint…', 'Budget', 'Manager tree', 'Fleet']) enabled(l);
  disabled('Archive earlier seat…');
  disabled('Up a level');
  btn('Halt').click();
  await tick();
  assert.equal(called('pause_session').at(-1)?.args.level, 'soft');

  daemon();
  btn('Interrupt now…').click();
  await tick();
  assert.match(dialogText(openDialog())[0], /^Interrupt .* now\?$/);
  await confirm();
  assert.equal(called('pause_session').at(-1)?.args.level, 'hard', 'one click and a confirm, no second key press');

  daemon();
  btn('Talk to the seat…').click();
  await tick();
  await confirm('hello');
  assert.deepEqual(called('send_message').at(-1)?.args, { sessionId: S, text: 'hello' });

  await selectRow(1);
  disabled('Halt');
  enabled('Open project');
});

test('budget: Raise one step asks first; a delegate row says why it cannot', async () => {
  daemon();
  managers.runManagers('mgr-budget', {});
  await tick();
  assert.equal(view().querySelector('.mgr-title').textContent, 'Budget: every manager’s limits');
  enabled('Raise one step').click();
  await tick();
  const d = openDialog();
  assert.match(dialogText(d)[0], /^Set global /);
  d.querySelectorAll('button').find((b) => b.textContent === 'Cancel').click();
  await tick();
  await selectRow(1);
  assert.match(disabled('Raise one step').title, /delegate/);
  disabled('Set a limit…');
});

test('fleet: Sort lists the columns as buttons; Reverse, Group, Window and Copy session ID by click', async () => {
  daemon();
  managers.runManagers('mgr-fleet', {});
  await tick();
  const meta = () => view().querySelector('.mgr-meta').textContent;
  enabled('Sort…').click();
  await tick();
  enabled('Sort by provider').click();
  await tick();
  assert.match(meta(), /sort provider/);
  enabled('Reverse').click();
  await tick();
  assert.match(meta(), /sort provider ↓/);
  enabled('Copy session ID').click();
  await tick();
  assert.deepEqual(copied, [S]);
  enabled('Window').click();
  await tick();
  enabled('Group').click();
  await tick();
  assert.match(meta(), /usage by/);
  disabled('Copy session ID');
});

test('lists: the empty portfolio list explains itself', async () => {
  daemon();
  managers.runManagers('mgr-portfolio', {});
  await tick();
  assert.equal(view().querySelector('.mgr-item .mgr-cell').textContent, NO_PORTFOLIO_NODES);
  disabled('Open session');
});

// ---------- the Vim-on keys still work ----------

test('with Vim keys on, s still saves the policy draft and a still opens the answer dialog', async () => {
  daemon();
  managers.runManagers('mgr-policy', {});
  await tick();
  const at = POLICY_FIELDS.findIndex((f) => f.kind === 'cycle' || f.kind === 'bool');
  await selectRow(at);
  btn('Edit field').click();
  await tick();
  doc.activeElement = view().querySelector('.mgr-list');
  const press = (key) => { for (const f of keyListeners) f({ key, target: doc.activeElement, preventDefault() {}, stopImmediatePropagation() {} }); };
  press('s');
  await tick();
  assert.ok(called('harness_manager_configure_policy').length, 's saved');
  managers.runManagers('mgr-close', {});
});

// Local #330: the list that holds focus is a named listbox whose aria-activedescendant follows the cursor, and h / l (which move
// the decision option, not the row) point it at the option until the row moves again.
test('manager list: a listbox that announces the current row, and the decision option while h / l move it', async () => {
  daemon();
  managers.runManagers('mgr-decisions', {});
  await tick();
  const list = view().querySelector('.mgr-list');
  const rowId = () => list.getAttribute('aria-activedescendant');
  const byId = (id) => body.walk().find((n) => n.id === id);
  assert.equal(list.getAttribute('role'), 'listbox');
  assert.ok(list.getAttribute('aria-label'), 'the list is named');
  const selected = list.querySelectorAll('.mgr-item').filter((r) => r.getAttribute('aria-selected') === 'true');
  assert.equal(selected.length, 1, 'one selected row');
  assert.equal(byId(rowId()), selected[0], 'the list points at the selected row');
  assert.ok(selected[0].className.includes('sel'));
  doc.activeElement = list;
  const press = (key) => { for (const f of keyListeners) f({ key, target: doc.activeElement, preventDefault() {}, stopImmediatePropagation() {} }); };
  const options = () => view().querySelectorAll('.mgr-options li');
  assert.ok(options().length >= 2, 'the decision has options');
  assert.equal(view().querySelector('.mgr-options').getAttribute('role'), 'listbox');
  press('l');
  await tick();
  const moved = byId(rowId());
  assert.equal(moved.getAttribute('role'), 'option');
  assert.equal(moved.getAttribute('aria-selected'), 'true');
  assert.equal(moved, options().find((o) => o.dataset.option === '1'), 'l points at the next option');
  press('h');
  await tick();
  assert.equal(byId(rowId()), options().find((o) => o.dataset.option === '0'), 'h points back at the first option');
  managers.runManagers('mgr-close', {});
});
