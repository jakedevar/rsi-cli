// Archive and restore for a first-time user (local #268): an archived session's header shows a plainly labelled Restore
// button in place of the dead Archive button, and archiving shows a toast with Undo that restores it. Both go through the
// unarchive command the U key, the menu and the palette already use, and both are real buttons Tab reaches.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  ARCHIVE_CLEARED_TOAST, ARCHIVE_UNDO_MS, WILL_ARCHIVE_TOAST, archiveButton, archiveConfirmText, archiveControls, archivePending, archiveWhenFinishedConfirmText,
  archivedToastText, defersArchive,
} from '../dist/approvals-model.js';
import { unavailable } from '../dist/registry.js';
import { LIFECYCLE_KEY_BINDINGS } from '../dist/actions/lifecycle-spec.js';
import { ARCHIVED_PLACEHOLDER } from '../dist/keylabel.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const html = readFileSync(here('../static/index.html'), 'utf8');

/** A small DOM: the parts el(), the toast and the confirm dialog touch, with listeners that can be fired. */
class Node {
  constructor(tag) {
    this.tagName = tag.toUpperCase(); this.children = []; this.dataset = {}; this.attrs = {}; this.listeners = {};
    this.className = ''; this.disabled = false; this._text = ''; this.tabIndex = -1; this.hidden = false; this.title = ''; this.id = '';
    this.parent = null; this.closed = false;
    const classes = new Set();
    this.classList = { toggle: (c, on) => { if (on) classes.add(c); else classes.delete(c); return on; }, contains: (c) => classes.has(c) };
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  replaceChildren(...nodes) { this._text = ''; this.children = []; this.append(...nodes); }
  append(...nodes) { for (const n of nodes) { n.parent = this; this.children.push(n); } }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  addEventListener(type, cb) { (this.listeners[type] ??= []).push(cb); }
  fire(type) { for (const cb of this.listeners[type] ?? []) cb({ target: this }); }
  click() { this.fire('click'); }
  focus() { document.activeElement = this; }
  closest() { return null; }
  querySelector(sel) { return this.all((n) => (sel === 'details' ? n.tagName === 'DETAILS' : n.className.split(' ').includes(sel.replace(/^.*\./, '')))).at(0) ?? null; }
  all(pred, out = []) { for (const c of this.children) { if (pred(c)) out.push(c); c.all(pred, out); } return out; }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this); }
  hidePopover() {}
  showPopover() {}
  // The fake confirm dialog: opening it presses the confirm button, as a user would.
  showModal() {
    queueMicrotask(() => {
      const form = this.children[0];
      form.fire('submit');
      this.fire('close');
    });
  }
  close() { this.fire('close'); }
}

const delays = [];
const calls = [];
const byId = {};
for (const id of ['toast', 'hard-pause', 'clear-pause', 'archive', 'restore', 'pause-state']) { byId[id] = new Node('button'); byId[id].id = id; }
byId.restore.hidden = true;
byId.restore.textContent = 'Restore';
byId.archive.textContent = 'Archive';
const body = new Node('body');
globalThis.document = {
  createElement: (t) => new Node(t),
  getElementById: (id) => byId[id] ?? null,
  querySelectorAll: () => [],
  querySelector: (sel) => byId.toast.querySelector(sel),
  body, activeElement: body,
};
globalThis.window = { setTimeout: (fn, ms) => { delays.push(ms); return delays.length; }, clearTimeout: () => {} };
globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => { calls.push([cmd, args]); return cmd === 'get_operator_pause' ? { pause_level: 'none' } : null; } } };

const { initApprovals, renderControls, archive } = await import('../dist/approvals.js');
const { toast } = await import('../dist/dom.js');
const { state, selected, isReadOnly, isDeparted } = await import('../dist/state.js');

const session = (status, over = {}) => ({ id: '11111111-aaaa-4bbb-8ccc-000000000001', status, title: 'Fake worker', query: 'q', provider: 'Claude', working_dir: '/w', ...over });
let current;
let refreshes = 0;
let redraws = 0;
// The real page draws from the shared state; here the header buttons are redrawn from what the pane would show.
let fromState = false;
const selectedNow = () => (fromState ? state.sessions.find((x) => x.id === current.id) : current);
initApprovals({ toast, refresh: () => { refreshes++; }, selected: () => selectedNow(), redraw: () => { redraws++; } });

test('an archived session shows Restore in place of Archive; every other session keeps Archive and hides Restore', () => {
  for (const status of ['Running', 'Completed', 'Failed', 'Interrupted']) {
    assert.deepEqual(archiveControls(status, false), { restoreHidden: true, restoreDisabled: false, archiveHidden: false, archiveDisabled: false }, status);
  }
  assert.deepEqual(archiveControls('Archived', false), { restoreHidden: false, restoreDisabled: false, archiveHidden: true, archiveDisabled: false });
  assert.equal(archiveControls('Deleted', false).archiveDisabled, true);
  assert.equal(archiveControls('Deleted', false).restoreHidden, true);
});

test('the header buttons follow the open session, and turn off with a reason while rsid is down', () => {
  state.daemonDown = false;
  current = session('Archived');
  renderControls(current);
  assert.equal(byId.restore.hidden, false);
  assert.equal(byId.restore.disabled, false);
  assert.equal(byId.archive.hidden, true);
  current = session('Running');
  renderControls(current);
  assert.equal(byId.restore.hidden, true);
  assert.equal(byId.archive.hidden, false);
  state.daemonDown = true;
  current = session('Archived');
  renderControls(current);
  assert.equal(byId.restore.disabled, true);
  assert.match(byId.restore.title, /rsid/);
  state.daemonDown = false;
  renderControls(current);
  assert.equal(byId.restore.disabled, false);
});

test('Restore is a plain, primary button wired to the unarchive action the U key runs, and the keys still work', () => {
  const tag = html.match(/<button id="restore"[^>]*>([^<]*)<\/button>/);
  assert.ok(tag, 'the header has a Restore button');
  assert.equal(tag[1], 'Restore');
  assert.match(tag[0], /class="btn primary"/);
  assert.match(tag[0], /type="button"/);
  assert.match(tag[0], /data-action="unarchive-session"/);
  assert.match(JSON.stringify(LIFECYCLE_KEY_BINDINGS.U), /unarchive-session/, 'the U key still runs the same action');
  // The button's action can run exactly when the session open in the view is archived.
  const snap = (status) => ({
    open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'archived', vimKeys: false, hideTools: false,
    showFinished: false, modalOpen: false, event: null, lifecycle: { target: { status }, questions: 0, notices: 0, completions: 0 },
  });
  assert.equal(unavailable('unarchive-session', snap('Archived')), null);
  assert.notEqual(unavailable('unarchive-session', snap('Running')), null);
});

test('the archived composer says what to do, in plain words', () => {
  assert.equal(ARCHIVED_PLACEHOLDER, 'Archived: Restore it to message it');
  assert.match(readFileSync(here('../src/sessions.ts'), 'utf8'), /ro \? \(s\.status === 'Archived' \|\| !isDeparted\(s\) \? ARCHIVED_PLACEHOLDER/);
});

test('Archive asks where the session goes, then toasts with an Undo button that restores that session', async () => {
  current = session('Completed');
  calls.length = 0; delays.length = 0; refreshes = 0;
  await archive();
  assert.deepEqual(calls[0], ['archive_session', { sessionId: current.id }]);
  const t = byId.toast;
  assert.equal(t.textContent, `${archivedToastText('Fake worker')}Undo`);
  assert.match(archiveConfirmText('Fake worker'), /moves to the Archived tab; nothing is deleted/);
  const undo = t.all((n) => n.tagName === 'BUTTON')[0];
  assert.equal(undo.textContent, 'Undo');
  assert.equal(undo.tabIndex, 0, 'a Tab stop');
  assert.equal(undo.type, 'button', 'never submits a form');
  assert.equal(delays.at(-1), ARCHIVE_UNDO_MS, 'the toast stays several seconds, not the usual 2.5');
  assert.ok(ARCHIVE_UNDO_MS >= 5000 && ARCHIVE_UNDO_MS <= 15000);
  assert.equal(refreshes, 1);
  assert.equal(globalThis.document.activeElement, undo, 'focus was on nothing, so Undo takes it: Tab is not a long walk away');
  calls.length = 0;
  undo.click();
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(calls[0], ['unarchive_session', { sessionId: '11111111-aaaa-4bbb-8ccc-000000000001' }]);
  assert.equal(t.textContent, 'Restored "Fake worker" to the session list.');
  assert.equal(refreshes, 2, 'the list is re-read after Undo');
});

test('Archive does not take focus from a control the user is on, and a cancelled Archive shows no toast', async () => {
  current = session('Completed');
  const other = new Node('button');
  other.focus();
  byId.toast.textContent = '';
  await archive();
  assert.equal(globalThis.document.activeElement, other);
  // Cancel: the fake dialog is closed without a submit.
  const showModal = Node.prototype.showModal;
  Node.prototype.showModal = function cancel() { queueMicrotask(() => this.fire('close')); };
  try {
    calls.length = 0; byId.toast.textContent = '';
    await archive();
    assert.equal(calls.length, 0, 'nothing is sent');
    assert.equal(byId.toast.textContent, '');
  } finally { Node.prototype.showModal = showModal; }
});

test('a failed Undo says so', async () => {
  current = session('Completed');
  globalThis.__TAURI__.core.invoke = async (cmd) => { if (cmd === 'unarchive_session') throw 'rsid is not answering'; return null; };
  await archive();
  byId.toast.all((n) => n.tagName === 'BUTTON')[0].click();
  await new Promise((r) => setTimeout(r, 0));
  assert.match(byId.toast.textContent, /Undo failed/);
});

// ---------- Archive on a running session: mark it to archive when it finishes (local #308) ----------

const sent = () => calls.filter((c) => c[0] !== 'get_operator_pause');
const ID = '11111111-aaaa-4bbb-8ccc-000000000001';
const resetInvoke = () => { globalThis.__TAURI__.core.invoke = async (cmd, args) => { calls.push([cmd, args]); return cmd === 'get_operator_pause' ? { pause_level: 'none' } : null; }; };
/** Opens every confirm dialog the way the user answers it: `answer` true presses the confirm button, false closes it. */
const answerConfirms = (answer) => {
  const asked = [];
  const showModal = Node.prototype.showModal;
  Node.prototype.showModal = function ask() {
    asked.push(this.all((n) => n.tagName === 'P').map((n) => n.textContent).join(' '));
    queueMicrotask(() => { if (answer) this.children[0].fire('submit'); this.fire('close'); });
  };
  return { asked, restore: () => { Node.prototype.showModal = showModal; } };
};

test('only a live (Starting, Running or WaitingApproval) session defers Archive; the button says what it will do', () => {
  for (const status of ['Starting', 'Running', 'WaitingApproval']) assert.equal(defersArchive(status), true, status);
  for (const status of ['Completed', 'Failed', 'Interrupted', 'Archived', 'Deleted']) assert.equal(defersArchive(status), false, status);
  assert.equal(archivePending({ status: 'Running', pending_archive: true }), true);
  assert.equal(archivePending({ status: 'Running', pending_archive: false }), false);
  assert.equal(archivePending({ status: 'Completed', pending_archive: true }), false, 'the mark means nothing once it has finished');
  assert.equal(archiveButton({ status: 'Completed' }).label, 'Archive');
  assert.equal(archiveButton({ status: 'Running' }).label, 'Archive when finished');
  assert.equal(archiveButton({ status: 'Running', pending_archive: true }).label, 'Cancel auto-archive');
  assert.equal(archiveButton({ status: 'WaitingApproval' }).label, 'Archive when finished');
  assert.equal(archiveButton({ status: 'WaitingApproval', pending_archive: true }).label, 'Cancel auto-archive');
  assert.equal(archivePending({ status: 'WaitingApproval', pending_archive: true }), true);
  assert.match(archiveWhenFinishedConfirmText('Fake worker'), /archives itself when this turn finishes; nothing is deleted/);
  assert.equal(WILL_ARCHIVE_TOAST, 'Will archive when complete', 'the TUI toast, word for word');
  assert.equal(ARCHIVE_CLEARED_TOAST, 'Auto-archive on completion cleared');
});

test('the header button follows the open session: Archive, Archive when finished, Cancel auto-archive', () => {
  state.daemonDown = false;
  renderControls(session('Completed'));
  assert.equal(byId.archive.textContent, 'Archive');
  renderControls(session('Running'));
  assert.equal(byId.archive.textContent, 'Archive when finished');
  assert.match(byId.archive.title, /cannot be archived yet/);
  assert.equal(byId.archive.classList.contains('pending'), false);
  renderControls(session('Running', { pending_archive: true }));
  assert.equal(byId.archive.textContent, 'Cancel auto-archive');
  assert.equal(byId.archive.classList.contains('pending'), true);
  renderControls(session('WaitingApproval'));
  assert.equal(byId.archive.textContent, 'Archive when finished', 'a session waiting on a question is still live, so Archive defers');
  renderControls(session('WaitingApproval', { pending_archive: true }));
  assert.equal(byId.archive.textContent, 'Cancel auto-archive');
  renderControls(session('Completed'));
  assert.equal(byId.archive.textContent, 'Archive');
});

test('Archive on a WaitingApproval session asks, sends MarkPendingArchive (never ArchiveSession) and toasts', async () => {
  resetInvoke();
  fromState = true;
  current = session('WaitingApproval');
  state.sessions = [current];
  state.departed = null;
  calls.length = 0; byId.toast.textContent = '';
  const confirms = answerConfirms(true);
  try { await archive(); } finally { confirms.restore(); }
  assert.equal(confirms.asked.length, 1);
  assert.match(confirms.asked[0], /archives itself when this turn finishes/);
  assert.deepEqual(sent(), [['mark_pending_archive', { sessionId: ID, pending: true }]]);
  assert.equal(byId.toast.textContent, WILL_ARCHIVE_TOAST);
  assert.equal(state.sessions[0].pending_archive, true);
  assert.equal(state.departed, null);
});

test('Archive on a Running session asks, sends MarkPendingArchive (never ArchiveSession), toasts, and shows the mark at once', async () => {
  resetInvoke();
  fromState = true;
  current = session('Running');
  state.sessions = [current];
  state.departed = null;
  calls.length = 0; refreshes = 0; redraws = 0; byId.toast.textContent = '';
  const confirms = answerConfirms(true);
  try { await archive(); } finally { confirms.restore(); }
  assert.equal(confirms.asked.length, 1);
  assert.match(confirms.asked[0], /archives itself when this turn finishes/);
  assert.deepEqual(sent(), [['mark_pending_archive', { sessionId: ID, pending: true }]]);
  assert.equal(byId.toast.textContent, WILL_ARCHIVE_TOAST);
  assert.equal(state.sessions[0].pending_archive, true, 'the list copy carries the mark before the next poll');
  assert.equal(state.departed, null, 'the session did not leave the list');
  assert.equal(redraws, 1);
  assert.equal(refreshes, 1);
});

test('Archive on a Running session that is marked clears the mark with one press and no question', async () => {
  fromState = true;
  const confirms = answerConfirms(true);
  calls.length = 0; byId.toast.textContent = '';
  try { await archive(); } finally { confirms.restore(); }
  assert.deepEqual(confirms.asked, [], 'clearing needs no confirmation');
  assert.deepEqual(sent(), [['mark_pending_archive', { sessionId: ID, pending: false }]]);
  assert.equal(byId.toast.textContent, ARCHIVE_CLEARED_TOAST);
  assert.equal(state.sessions[0].pending_archive, false);
});

test('a declined Archive-when-finished sends nothing, and a refused one says so and leaves the mark alone', async () => {
  fromState = true;
  current = session('Running');
  state.sessions = [current];
  calls.length = 0; byId.toast.textContent = '';
  let confirms = answerConfirms(false);
  try { await archive(); } finally { confirms.restore(); }
  assert.deepEqual(sent(), []);
  assert.equal(byId.toast.textContent, '');
  globalThis.__TAURI__.core.invoke = async (cmd, args) => { calls.push([cmd, args]); if (cmd === 'mark_pending_archive') throw 'Session is not found in active sessions'; return null; };
  confirms = answerConfirms(true);
  try { await archive(); } finally { confirms.restore(); }
  assert.match(byId.toast.textContent, /Mark pending archive failed: .*not found in active sessions/);
  assert.equal(state.sessions[0].pending_archive, undefined);
  resetInvoke();
});

test('Archive on a finished session still sends ArchiveSession, and the pane shows Archived at once (local #309)', async () => {
  resetInvoke();
  fromState = false;
  current = session('Completed');
  state.sessions = [current];
  state.selectedId = ID;
  state.view = 'active';
  state.departed = null;
  calls.length = 0; redraws = 0; refreshes = 0;
  const confirms = answerConfirms(true);
  try { await archive(); } finally { confirms.restore(); }
  assert.deepEqual(sent(), [['archive_session', { sessionId: ID }]]);
  assert.equal(state.departed.status, 'Archived');
  assert.equal(redraws, 1, 'drawn before the list is read again');
  assert.ok(redraws === 1 && refreshes === 1);
  // The poll that follows no longer lists it; the open session is the one that left.
  state.sessions = [];
  const open = selected();
  assert.equal(open.status, 'Archived');
  assert.equal(isDeparted(open), true);
  assert.equal(isReadOnly(open), true, 'no message box, no Stop');
  const ac = archiveControls(open.status, false);
  assert.deepEqual([ac.restoreHidden, ac.archiveHidden], [false, true], 'Restore, not Archive');
});

test('a refused ArchiveSession leaves the pane as it was, and Undo puts the session back as it was', async () => {
  fromState = false;
  current = session('Completed');
  state.sessions = [current];
  state.selectedId = ID;
  state.departed = null;
  globalThis.__TAURI__.core.invoke = async (cmd) => { if (cmd === 'archive_session') throw 'Cannot archive active session'; return null; };
  byId.toast.textContent = '';
  let confirms = answerConfirms(true);
  try { await archive(); } finally { confirms.restore(); }
  assert.match(byId.toast.textContent, /Archive failed: .*Cannot archive active session/);
  assert.equal(state.departed, null, 'nothing changed, so nothing is shown as archived');
  assert.equal(selected(), current);
  // Archive for real, then Undo.
  resetInvoke();
  confirms = answerConfirms(true);
  try { await archive(); } finally { confirms.restore(); }
  state.sessions = [];
  assert.equal(selected().status, 'Archived');
  const undo = byId.toast.all((n) => n.tagName === 'BUTTON')[0];
  calls.length = 0;
  undo.click();
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(calls[0], ['unarchive_session', { sessionId: ID }]);
  assert.equal(state.departed, null);
  assert.equal(selected()?.status, 'Completed', 'the pane is the live session again before the poll');
  state.selectedId = null;
  state.sessions = [];
});

test('a departed session is only the open one in the Active view', () => {
  state.sessions = [];
  state.selectedId = ID;
  state.departed = session('Archived');
  state.view = 'active';
  assert.equal(selected()?.id, ID);
  state.view = 'archived';
  assert.equal(selected(), undefined, 'the Archived view lists what it lists');
  state.view = 'active';
  state.selectedId = '22222222-aaaa-4bbb-8ccc-000000000002';
  assert.equal(selected(), undefined, 'another selection is not the departed one');
  state.selectedId = null;
  state.departed = null;
});
