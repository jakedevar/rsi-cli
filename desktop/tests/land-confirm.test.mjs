// Landing a session (#209): every path opens a plain-words confirm dialog; Land sends the same
// RPC as before, Cancel and Escape send nothing, and the settled messages are jargon-free.
// The DOM side runs against a minimal fake document (no browser needed).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { SESSIONOPS_ACTIONS } from '../dist/actions/sessionops.js';
import { LAND_CHORD, advanceChord, confirmKey } from '../dist/confirm-model.js';
import {
  LAND_CANCEL_LABEL, LAND_CONFIRM_LABEL, LAND_TOOLTIP, landBlocker, landConfirmCopy, landName, landProgressMessage,
  landQueuedMessage, landStage, settledMessage, stageNotes,
} from '../dist/sessionops-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

// ---------- fake DOM ----------

class FakeEl {
  constructor(tag) { this.tag = tag; this.children = []; this.listeners = {}; this.textContent = ''; this.className = ''; this.attrs = {}; this.parent = null; }
  append(...kids) { for (const k of kids) { k.parent = this; this.children.push(k); } }
  replaceChildren(...kids) { this.textContent = ''; this.children = []; this.append(...kids); }
  querySelector() { return null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  fire(t, ev = {}) { const e = { target: this, preventDefault() { e.prevented = true; }, ...ev }; for (const f of this.listeners[t] ?? []) f(e); return e; }
  closest(sel) { for (let n = this; n; n = n.parent) if (n.tag === sel) return n; return null; }
  remove() { this.removed = true; }
  focus() { FakeEl.focused = this; }
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  texts() { return this.walk().map((n) => n.textContent).filter(Boolean); }
  button(label) { return this.walk().find((n) => n.tag === 'button' && n.textContent === label); }
  // dialog
  showModal() { this.open = true; }
  close() { if (!this.open) return; this.open = false; this.fire('close'); }
  // form
  requestSubmit() { this.fire('submit'); this.parent?.fire; }
}
const dialogs = [];
const toasts = [];
const realSetInterval = globalThis.setInterval;
globalThis.setInterval = (...a) => realSetInterval(...a).unref(); // the queue watch must not keep the test process alive
globalThis.window = globalThis;
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };
globalThis.addEventListener = () => {};
globalThis.document = {
  documentElement: { dataset: {} }, title: '',
  addEventListener() {},
  querySelector: () => null, querySelectorAll: () => [],
  getElementById: () => new FakeEl('div'),
  createElement: (tag) => {
    const e = new FakeEl(tag);
    if (tag === 'dialog') dialogs.push(e);
    if (tag === 'form') e.requestSubmit = function () { this.fire('submit'); const d = this.parent; if (d) d.close(); };
    return e;
  },
  body: new FakeEl('body'),
};

const approvals = await import('../dist/approvals.js');
const run = await import('../dist/actions/sessionops-run.js');
const { state } = await import('../dist/state.js');
const { onToast } = await import('../dist/dom.js');
onToast((msg, ok) => toasts.push({ msg, ok }));

const SOURCE = { session_id: 's1', branch: 'rsi/abc', commit: 'c'.repeat(40), ahead: 2, on_rolling: false, dirty: false, title: 'Fix login' };
const SESSION = { id: 's1', status: 'Completed', session_kind: 'Standard', title: 'Fix login', git_branch: 'rsi/abc' };

/** Run the land action with a scripted daemon; returns the recorded invoke calls. */
function setup(responses = {}) {
  const calls = [];
  globalThis.__TAURI__ = {
    core: {
      invoke: async (cmd, args) => {
        calls.push({ cmd, args });
        if (cmd === 'land_session') {
          if (args.dryRun) return { source: SOURCE, entry: null };
          return responses.land ?? { source: SOURCE, entry: { id: 'e1', sequence: 4, source_commit: 'c'.repeat(40), state: 'queued', outcome: null } };
        }
        if (cmd === 'rolling_queue_entries') return { entries: responses.entries ?? [] };
        return {};
      },
    },
  };
  state.sessions = [SESSION];
  state.selectedId = 's1';
  dialogs.length = 0;
  toasts.length = 0;
  return calls;
}
const tick = () => new Promise((r) => setTimeout(r, 0));
const inv = (source) => ({ source });
const landCalls = (calls) => calls.filter((c) => c.cmd === 'land_session');

// ---------- the dialog opens from every path ----------

for (const source of ['menu', 'context-menu', 'palette', 'button', 'key', 'command']) {
  test(`landing from ${source} opens the confirm dialog and sends no queue request yet`, async () => {
    const calls = setup();
    run.runSessionOps('land-session', inv(source));
    await tick(); await tick();
    assert.equal(dialogs.length, 1, 'a confirm dialog opens');
    assert.ok(dialogs[0].open);
    assert.deepEqual(landCalls(calls).map((c) => c.args.dryRun), [true], 'only the dry run so far');
    dialogs[0].close();
    await tick();
  });
}

test('the dialog says in plain words what landing does and names the session and branch', async () => {
  setup();
  run.runSessionOps('land-session', inv('menu'));
  await tick(); await tick();
  const dlg = dialogs[0];
  const text = dlg.texts().join('\n');
  assert.match(text, /adds this session’s work to the shared main line after automatic checks/);
  assert.match(text, /Session: Fix login/);
  assert.match(text, /Branch: rsi\/abc/);
  assert.ok(dlg.button(LAND_CONFIRM_LABEL) && dlg.button(LAND_CANCEL_LABEL), 'Land and Cancel buttons');
  assert.doesNotMatch(text, /rolling|queue|\bsha\b|unmerged|commits? ahead|double|again within/i);
  dlg.close();
  await tick();
});

// ---------- Cancel and Escape do nothing; Land sends the same RPC ----------

test('Cancel sends nothing and shows no toast', async () => {
  const calls = setup();
  run.runSessionOps('land-session', inv('menu'));
  await tick(); await tick();
  dialogs[0].button('Cancel').fire('click');
  await tick(); await tick();
  assert.deepEqual(landCalls(calls).map((c) => c.args.dryRun), [true]);
  assert.equal(toasts.length, 0);
});

test('Escape closes the dialog without confirming and sends nothing', async () => {
  const calls = setup();
  run.runSessionOps('land-session', inv('key'));
  await tick(); await tick();
  dialogs[0].close(); // the browser's Escape closes a modal dialog with no submit
  await tick(); await tick();
  assert.deepEqual(landCalls(calls).map((c) => c.args.dryRun), [true]);
  assert.equal(toasts.length, 0);
});

test('Land sends the same enqueue request as before and says it is queued', async () => {
  const calls = setup();
  run.runSessionOps('land-session', inv('menu'));
  await tick(); await tick();
  dialogs[0].walk().find((n) => n.tag === 'form').requestSubmit();
  await tick(); await tick();
  const sent = landCalls(calls);
  assert.equal(sent.length, 2);
  assert.deepEqual(sent[0].args, { sessionId: 's1', commit: null, idempotencyKey: null, dryRun: true });
  assert.equal(sent[1].args.sessionId, 's1');
  assert.equal(sent[1].args.commit, 'c'.repeat(40));
  assert.match(sent[1].args.idempotencyKey, /^operator-land-/);
  assert.equal(sent[1].args.dryRun, false);
  assert.equal(toasts.at(-1).msg, landQueuedMessage('Fix login'));
  assert.equal(toasts.at(-1).ok, true);
});

test('Enter on the dialog confirms; the chord <Space>gl confirms with the Vim keys on', async () => {
  for (const [name, keys] of [['Enter', [{ key: 'Enter' }]], ['chord', [{ key: ' ' }, { key: 'g' }, { key: 'l' }]]]) {
    const calls = setup();
    run.runSessionOps('land-session', inv('key'));
    await tick(); await tick();
    for (const k of keys) dialogs[0].fire('keydown', k);
    await tick(); await tick();
    assert.equal(landCalls(calls).length, 2, name);
  }
});

test('with the Vim keys off the chord does nothing but Enter still confirms', async () => {
  const yes = async (vimKeysOn, keys) => {
    dialogs.length = 0;
    const p = approvals.confirmDialog('m', 'Land', { chord: LAND_CHORD, vimKeysOn: () => vimKeysOn });
    for (const k of keys) dialogs[0].fire('keydown', k);
    if (dialogs[0].open) dialogs[0].close();
    return p;
  };
  const chord = [{ key: ' ' }, { key: 'g' }, { key: 'l' }];
  assert.equal(await yes(true, chord), true);
  assert.equal(await yes(false, chord), false);
  assert.equal(await yes(false, [{ key: 'Enter' }]), true);
  assert.equal(await yes(true, [{ key: 'x' }, { key: 'Escape' }]), false);
});

test('the chord and Enter rules are pure and keep modifiers out', () => {
  const press = (st, key, extra = {}) => confirmKey(st, { key, targetIsButton: false, ...extra }, { vimKeysOn: true, chord: LAND_CHORD });
  let r = press({ progress: 0 }, ' '); assert.deepEqual(r, { st: { progress: 1 }, confirm: false });
  r = press(r.st, 'g'); r = press(r.st, 'l'); assert.equal(r.confirm, true);
  assert.equal(press({ progress: 0 }, 'Enter', { targetIsButton: true }).confirm, false, 'a focused button keeps its own press');
  assert.equal(press({ progress: 0 }, 'Enter').confirm, true);
  assert.equal(press({ progress: 2 }, 'l', { ctrlKey: true }).confirm, false);
  assert.deepEqual(advanceChord(1, { key: ' ' }, LAND_CHORD), { progress: 1, done: false });
  assert.deepEqual(advanceChord(2, { key: 'x' }, LAND_CHORD), { progress: 0, done: false });
});

test('a session already on the main line shows a plain message and no dialog', async () => {
  const calls = setup();
  const was = globalThis.__TAURI__.core.invoke;
  globalThis.__TAURI__.core.invoke = async (cmd, args) => (cmd === 'land_session' && args.dryRun ? { source: { ...SOURCE, on_rolling: true } } : was(cmd, args));
  run.runSessionOps('land-session', inv('menu'));
  await tick(); await tick();
  assert.equal(dialogs.length, 0);
  assert.equal(toasts.at(-1).msg, landBlocker({ ...SOURCE, on_rolling: true }));
  assert.equal(landCalls(calls).length, 0);
});

// ---------- copy ----------

test('the dialog copy is jargon-free and handles missing names', () => {
  const copy = landConfirmCopy(SOURCE, SESSION);
  assert.equal(copy.title, 'Land this session?');
  assert.deepEqual(copy.detail, ['Session: Fix login', 'Branch: rsi/abc']);
  assert.match(copy.message, /2 changes are ready to land/);
  assert.match(landConfirmCopy({ ...SOURCE, ahead: 1 }).message, /1 change is ready/);
  assert.match(landConfirmCopy({ ...SOURCE, dirty: true }).message, /not saved yet in its workspace is left out/);
  assert.equal(landName({ ...SOURCE, title: null, branch: null }, { id: 'abcdef012345', title: '  ', git_branch: null }), 'abcdef01');
  for (const c of [copy, landConfirmCopy({ ...SOURCE, dirty: true, ahead: null })]) {
    assert.doesNotMatch([c.title, c.message, ...c.detail.slice(0, 1)].join(' '), /rolling|queue|sha\b|unmerged|ahead/i);
  }
});

test('progress and outcome use four plain stages', () => {
  assert.deepEqual(['queued', 'admitted', 'gating', 'published', 'refused', 'failed', 'superseded'].map(landStage),
    ['queued', 'checking', 'checking', 'landed', 'refused', 'refused', 'refused']);
  assert.match(landQueuedMessage('T'), /^Queued “T” to land/);
  assert.match(landProgressMessage('T', 'checking'), /^Checking “T”/);
  const e = (state, outcome = null) => ({ id: 'e1', sequence: 7, source_commit: 'a'.repeat(40), state, outcome });
  assert.match(settledMessage('T', e('published', { landed_sha: 'b'.repeat(40) })), /^Landed “T”\./);
  const refused = settledMessage('T', e('refused', { refusal: 'The tests failed in the checks.' }));
  assert.equal(refused, 'Not landed: “T”. The tests failed in the checks.');
  assert.doesNotMatch(refused, /queue #|[0-9a-f]{9}/);
  assert.match(settledMessage('T', e('failed')), /^Not landed: “T”\. The automatic checks could not finish\./);
  const first = stageNotes([{ entryId: 'e1', label: 'T' }], [e('gating')]);
  assert.deepEqual(first.notes, [landProgressMessage('T', 'checking')]);
  assert.deepEqual(stageNotes(first.watching, [e('gating')]).notes, [], 'a stage is announced once');
});

test('a landing is watched until it settles and then reported in plain words', async () => {
  const entry = (state, outcome = null) => ({ id: 'e1', sequence: 4, source_commit: 'c'.repeat(40), state, outcome });
  assert.match(settledMessage('Fix login', entry('refused', { detail: 'Checks failed' })), /^Not landed: “Fix login”\. Checks failed$/);
});

test('the registry tooltip explains the action once without queue words, and the old double-press is gone', () => {
  const land = SESSIONOPS_ACTIONS.find((a) => a.id === 'land-session');
  assert.equal(land.detail, LAND_TOOLTIP);
  assert.doesNotMatch(land.detail, /rolling|queue|press again/i);
  const src = readFileSync(here('../src/actions/sessionops-run.ts'), 'utf8');
  assert.doesNotMatch(src, /armed|confirmsLand|LAND_CONFIRM_MS/);
  assert.match(src, /confirmDialog\(copy\.message/);
});
