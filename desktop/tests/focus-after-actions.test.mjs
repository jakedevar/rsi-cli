// Local #297: keyboard focus is never left on the page after an action removes or redraws the focused control.
// Copy ID in the session header, Submit answer / Let the agent decide on a question, the header buttons that hide or disable
// themselves (Resume, Restore, Archive, Stop), the Undo on a toast, a dialog whose opener is gone and the to-do panel's
// Hide. The real modules run against a small fake document that behaves like a browser's focus: an element that is removed,
// hidden or disabled stops being `document.activeElement` (focus fixup), so `activeElement` is the body.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

// ---------- the fake document ----------

class N {
  constructor(tag) {
    this.tagName = tag.toUpperCase(); this.children = []; this.parentNode = null; this.className = ''; this.id = '';
    this.hidden = false; this.disabled = false; this.dataset = {}; this.attrs = {}; this.style = {}; this.listeners = {};
    this._text = ''; this.tabIndex = -1; this.title = ''; this.value = ''; this.checked = false; this.open = false;
  }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { for (const c of this.children) c.parentNode = null; this.children = []; this._text = String(v); }
  get parentElement() { return this.parentNode; }
  get isConnected() { for (let n = this; n; n = n.parentNode) if (n === doc.body) return true; return false; }
  get classList() {
    const self = this;
    return {
      add: (c) => { if (!self.has(c)) self.className = `${self.className} ${c}`.trim(); },
      remove: (c) => { self.className = self.className.split(/\s+/).filter((x) => x && x !== c).join(' '); },
      toggle: (c, on) => { if (on ?? !self.has(c)) self.classList.add(c); else self.classList.remove(c); },
      contains: (c) => self.has(c),
    };
  }
  has(c) { return this.className.split(/\s+/).includes(c); }
  append(...kids) { for (const k of kids) { k.parentNode?.children.splice(k.parentNode.children.indexOf(k), 1); k.parentNode = this; this.children.push(k); } }
  replaceChildren(...kids) { for (const c of this.children) c.parentNode = null; this.children = []; this._text = ''; this.append(...kids); }
  remove() { this.parentNode?.children.splice(this.parentNode.children.indexOf(this), 1); this.parentNode = null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  contains(n) { for (let x = n; x; x = x.parentNode) if (x === this) return true; return false; }
  closest(sel) { for (let n = this; n; n = n.parentNode) if (n.matches(sel)) return n; return null; }
  matches(sel) {
    return sel.split(',').some((part) => {
      const m = /^\s*([a-z0-9]*)((?:\.[\w-]+)*)(?:\[(\w+)\])?\s*$/i.exec(part);
      if (!m) return false;
      const [, tag, cls, attr] = m;
      if (tag && this.tagName !== tag.toUpperCase()) return false;
      if (cls && !cls.split('.').filter(Boolean).every((c) => this.has(c))) return false;
      if (attr && !this[attr] && this.attrs[attr] === undefined) return false;
      return true;
    });
  }
  querySelectorAll(sel) { const out = []; const walk = (n) => { for (const c of n.children) { if (c.matches(sel)) out.push(c); walk(c); } }; walk(this); return out; }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  dispatchEvent(ev) {
    const e = Object.assign(ev, { target: ev.target ?? this, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } });
    for (let n = this; n; n = n.parentNode) for (const f of n.listeners[ev.type] ?? []) f(e);
    return e;
  }
  hiddenChain() { for (let n = this; n; n = n.parentNode) if (n.hidden) return true; return false; }
  focus() { if (this.isConnected && !this.disabled && !this.hiddenChain()) focused = this; }
  blur() { if (focused === this) focused = null; }
  click() { return this.dispatchEvent({ type: 'click', detail: 0 }); }
}
class D extends N {}

const findById = (root, id) => { for (const c of root.children) { if (c.id === id) return c; const r = findById(c, id); if (r) return r; } return null; };
let focused = null;
const byId = new Map();
const docListeners = {};
const doc = {
  body: new N('body'),
  createElement: (tag) => (tag === 'dialog' ? new D(tag) : new N(tag)),
  createTextNode: (t) => Object.assign(new N('#text'), { _text: t }),
  getElementById: (id) => byId.get(id) ?? findById(doc.body, id),
  querySelector: (s) => doc.body.querySelector(s),
  querySelectorAll: (s) => doc.body.querySelectorAll(s),
  addEventListener: (t, f) => { (docListeners[t] ??= []).push(f); },
  // Browser focus fixup: a focused element that is removed, hidden or disabled is no longer the active element.
  get activeElement() { return focused && focused.isConnected && !focused.hiddenChain() && !focused.disabled ? focused : doc.body; },
};
const timers = [];
globalThis.document = doc;
globalThis.HTMLElement = N;
globalThis.HTMLDialogElement = D;
globalThis.window = { setTimeout: (f) => { timers.push(f); return timers.length; }, clearTimeout() {}, matchMedia: () => ({ matches: false }) };
const calls = [];
globalThis.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === 'get_operator_pause') return { pause_level: 'none' };
      return null;
    },
  },
};
const flushTimers = () => { while (timers.length) timers.shift()(); };
const settle = () => new Promise((r) => setTimeout(r, 20));

/** A fresh page: the elements the modules look up by id, everything detached from the previous test. */
function page() {
  byId.clear(); doc.body.replaceChildren(); focused = null; timers.length = 0; calls.length = 0;
  const mk = (tag, id, parent = doc.body, extra = {}) => {
    const e = doc.createElement(tag);
    e.id = id; Object.assign(e, extra); byId.set(id, e); parent.append(e); return e;
  };
  const list = mk('ul', 'session-list', doc.body, { tabIndex: 0 });
  const view = mk('section', 'session-view');
  const head = mk('div', 's-head', view);
  const secondary = mk('div', 's-secondary', head);
  const actions = mk('div', 'head-actions', head);
  const clear = mk('button', 'clear-pause', actions, { hidden: true });
  const interrupt = mk('button', 'interrupt', actions);
  const pauseBtn = mk('button', 'hard-pause', actions);
  const restore = mk('button', 'restore', actions, { hidden: true });
  const archive = mk('button', 'archive', actions);
  const pauseState = mk('span', 'pause-state', actions, { hidden: true });
  const banner = mk('div', 'approval-banner', view, { hidden: true });
  const events = mk('div', 'events', view, { tabIndex: 0 });
  const message = mk('textarea', 'message', view);
  const toastEl = mk('div', 'toast');
  return { list, view, secondary, clear, interrupt, pauseBtn, restore, archive, pauseState, banner, events, message, toastEl };
}
const buttonNamed = (root, text) => root.querySelectorAll('button').find((b) => b.textContent === text);

const session = (over = {}) => ({ id: 's1abcdef-0000-0000-0000-000000000001', status: 'Running', provider: 'Claude', working_dir: '/w/rsi', git_branch: 'rsi/x', ...over });
const ask = (...qs) => ({ questions: qs.map((q) => ({ question: q, header: q, options: [{ label: 'Yes', description: '' }, { label: 'No', description: '' }] })) });

const { renderSecondary } = await import('../dist/session-secondary.js');
const { renderApproval, renderControls, initApprovals } = await import('../dist/approvals.js');
const { toast } = await import('../dist/dom.js');
const { state } = await import('../dist/state.js');
const { initDialogFocus } = await import('../dist/dialog-focus.js');
const { answerLanding, ANSWER_LAND_MS } = await import('../dist/approvals-model.js');
const { keepFocusIn, focusStranded, focusInside, landFocus } = await import('../dist/focus-return.js');

// ---------- Copy ID ----------

test('Copy ID keeps keyboard focus when the header redraws (each refresh rebuilds it)', () => {
  const p = page();
  let copied = null;
  const draw = () => renderSecondary(p.secondary, session(), (text, what) => { copied = { text, what }; });
  draw();
  const first = buttonNamed(p.secondary, 'Copy ID');
  first.focus();
  assert.equal(doc.activeElement, first);
  first.click(); // the delegated data-action click runs copy-session-id; the toast does not touch the header
  assert.equal(first.dataset.action, 'copy-session-id');
  draw(); // the next refresh rebuilds the header
  const second = buttonNamed(p.secondary, 'Copy ID');
  assert.notEqual(second, first, 'the button was replaced');
  assert.equal(doc.activeElement, second, 'focus is on its replacement, not the page');
  assert.equal(copied, null, 'the Details copy buttons are not the Copy ID action');
});

test('a Copy button inside Details keeps focus and the fold stays open across a redraw', () => {
  const p = page();
  const draw = () => renderSecondary(p.secondary, session(), () => {});
  draw();
  const details = p.secondary.querySelector('details');
  details.open = true;
  const folder = [...p.secondary.querySelectorAll('.detail-row')].find((r) => r.textContent.includes('Folder')).querySelector('button');
  folder.focus();
  draw();
  const now = [...p.secondary.querySelectorAll('.detail-row')].find((r) => r.textContent.includes('Folder')).querySelector('button');
  assert.notEqual(now, folder);
  assert.equal(doc.activeElement, now, 'the Folder row\'s Copy, not the Session ID row\'s');
  assert.equal(p.secondary.querySelector('details').open, true);
});

test('a header redraw leaves focus alone when it was not in the header', () => {
  const p = page();
  const draw = () => renderSecondary(p.secondary, session(), () => {});
  draw();
  p.list.focus();
  draw();
  assert.equal(doc.activeElement, p.list);
});

// ---------- Submit answer / Let the agent decide ----------

function banner(p, refreshTo) {
  initApprovals({ toast: () => {}, refresh: () => { renderApproval(refreshTo()); }, selected: () => undefined });
}

for (const [name, button] of [['Submit answer', 'Submit answer'], ['Let the agent decide', 'Let the agent decide']]) {
  test(`${name}: focus moves to the message box when the session can take a message`, async () => {
    const p = page();
    state.view = 'active';
    let current = session({ pending_question: ask('Pick one?') });
    banner(p, () => current);
    renderApproval(current);
    assert.equal(p.banner.hidden, false);
    const radio = p.banner.querySelector('input');
    radio.checked = true;
    const btn = buttonNamed(p.banner, button);
    btn.focus();
    assert.equal(doc.activeElement, btn);
    current = session({ pending_question: undefined }); // the daemon took the answer
    btn.click();
    await settle();
    assert.ok(calls.some((c) => c.cmd === 'answer_question'), 'the answer was sent');
    assert.equal(p.banner.hidden, true);
    assert.equal(doc.activeElement, p.message, 'the message box, not the page');
  });
}

test('a pointer click that left the page focused (macOS WebKit does not focus a clicked button) still lands in the message box', async () => {
  const p = page();
  state.view = 'active';
  let current = session({ pending_question: ask('Pick one?') });
  banner(p, () => current);
  renderApproval(current);
  p.banner.querySelector('input').checked = true;
  assert.equal(doc.activeElement, doc.body);
  current = session();
  buttonNamed(p.banner, 'Submit answer').click();
  await settle();
  assert.equal(doc.activeElement, p.message);
});

test('the next pending question gets focus when the session asks another', async () => {
  const p = page();
  state.view = 'active';
  let current = session({ pending_question: ask('First?') });
  banner(p, () => current);
  renderApproval(current);
  p.banner.querySelector('input').checked = true;
  const btn = buttonNamed(p.banner, 'Submit answer');
  btn.focus();
  current = session({ pending_question: ask('Second?') });
  btn.click();
  await settle();
  assert.equal(p.banner.hidden, false);
  assert.ok(p.banner.textContent.includes('Second?'));
  assert.equal(doc.activeElement, p.banner.querySelector('input'), 'the first option of the new question');
  assert.ok(p.banner.contains(doc.activeElement));
});

test('an archived (read-only) session takes focus to the conversation, not a disabled message box', async () => {
  const p = page();
  state.view = 'archived';
  try {
    p.message.disabled = true;
    let current = session({ status: 'Archived', pending_question: ask('Q?') });
    banner(p, () => current);
    // The banner is only shown for a live session; this one was live when it was built.
    renderApproval(session({ pending_question: ask('Q?') }));
    p.banner.querySelector('input').checked = true;
    const btn = buttonNamed(p.banner, 'Submit answer');
    btn.focus();
    current = session({ status: 'Archived' });
    btn.click();
    await settle();
    assert.equal(doc.activeElement, p.events);
  } finally { state.view = 'active'; }
});

test('focus the user moved elsewhere is not stolen when the banner goes away', () => {
  const p = page();
  state.view = 'active';
  renderApproval(session({ pending_question: ask('Q?') }));
  p.list.focus();
  renderApproval(session());
  assert.equal(p.banner.hidden, true);
  assert.equal(doc.activeElement, p.list);
});

test('a failed answer keeps the banner and its focus', async () => {
  const p = page();
  const failing = globalThis.__TAURI__.core.invoke;
  globalThis.__TAURI__.core.invoke = async () => { throw new Error('rsid down'); };
  try {
    state.view = 'active';
    const current = session({ pending_question: ask('Q?') });
    banner(p, () => current);
    renderApproval(current);
    p.banner.querySelector('input').checked = true;
    const btn = buttonNamed(p.banner, 'Submit answer');
    btn.focus();
    btn.click();
    await settle();
    assert.equal(doc.activeElement, btn);
  } finally { globalThis.__TAURI__.core.invoke = failing; }
});

test('answerLanding: the next question, then the message box, then the conversation, then the list', () => {
  assert.equal(answerLanding({ nextQuestion: true, sessionOpen: true, canMessage: true }), 'next-question');
  assert.equal(answerLanding({ nextQuestion: false, sessionOpen: true, canMessage: true }), 'composer');
  assert.equal(answerLanding({ nextQuestion: false, sessionOpen: true, canMessage: false }), 'conversation');
  assert.equal(answerLanding({ nextQuestion: false, sessionOpen: false, canMessage: false }), 'list');
  assert.ok(ANSWER_LAND_MS > 0);
});

// ---------- the header buttons that hide or disable themselves ----------

test('Restore, pressed from the keyboard, hands focus on when the session leaves the archive and the button hides', async () => {
  const p = page();
  initApprovals({ toast: () => {}, refresh: () => {}, selected: () => session() });
  state.view = 'active';
  renderControls(session({ status: 'Archived' }));
  await settle();
  assert.equal(p.restore.hidden, false);
  p.restore.focus();
  assert.equal(doc.activeElement, p.restore);
  renderControls(session({ status: 'Running' }));
  assert.equal(p.restore.hidden, true);
  assert.equal(doc.activeElement, p.message, 'the message box of the session now open');
});

test('Archive, pressed from the keyboard, hands focus on when it hides (an archived session reads only)', async () => {
  const p = page();
  initApprovals({ toast: () => {}, refresh: () => {}, selected: () => session() });
  renderControls(session());
  await settle();
  p.message.disabled = true;
  p.archive.focus();
  renderControls(session({ status: 'Archived' }));
  assert.equal(p.archive.hidden, true);
  assert.equal(doc.activeElement, p.events);
});

test('Stop this turn (disabled once the turn is stopped) and Pause do not strand focus', () => {
  const p = page();
  p.interrupt.focus();
  assert.equal(doc.activeElement, p.interrupt);
  keepFocusIn(p.view, () => { p.interrupt.disabled = true; }, ['message', 'events', 'session-list']);
  assert.equal(doc.activeElement, p.message);
  p.pauseBtn.focus();
  keepFocusIn(p.view, () => { p.pauseBtn.disabled = true; }, ['message', 'events', 'session-list']);
  assert.equal(doc.activeElement, p.message);
});

test('the whole session view going away (Restore from the archive, delete) lands on the session list', () => {
  const p = page();
  p.restore.hidden = false;
  p.restore.focus();
  keepFocusIn(p.view, () => { p.view.hidden = true; }, ['message', 'events', 'session-list']);
  assert.equal(doc.activeElement, p.list);
});

test('keepFocusIn leaves focus alone when the focused control survives or focus was elsewhere', () => {
  const p = page();
  p.message.focus();
  keepFocusIn(p.view, () => {}, ['session-list']);
  assert.equal(doc.activeElement, p.message);
  p.list.focus();
  keepFocusIn(p.view, () => { p.view.hidden = true; }, ['session-list']);
  assert.equal(doc.activeElement, p.list);
  assert.equal(focusStranded(), false);
  assert.equal(focusInside(p.view), false);
  assert.equal(landFocus([null, 'nope', 'message']), null, 'nothing usable: no throw, no change');
});

// ---------- toast actions ----------

test('Undo on the archive toast: pressing it, or the toast timing out, returns focus instead of dropping it', () => {
  const p = page();
  const ran = [];
  // The Archive button had focus, then the archived session's controls hid it; the confirmation put focus on Undo.
  p.archive.focus();
  toast('Moved "x" to Archived.', true, { label: 'Undo', ms: 8000, run: () => ran.push('undo') });
  p.archive.hidden = true;
  const undo = p.toastEl.querySelector('button');
  assert.equal(undo.textContent, 'Undo');
  undo.focus();
  assert.equal(doc.activeElement, undo);
  undo.click();
  assert.deepEqual(ran, ['undo']);
  assert.equal(p.toastEl.hidden, true);
  assert.equal(doc.activeElement, p.list, 'the opener (Archive) is gone and hidden: the session list');
});

test('a toast with Undo that times out while focused hands focus back to the control that opened it', () => {
  const p = page();
  p.interrupt.focus();
  toast('Moved to Archived.', true, { label: 'Undo', ms: 8000, run: () => {} });
  p.toastEl.querySelector('button').focus();
  flushTimers(); // the toast's hide timer
  assert.equal(p.toastEl.hidden, true);
  assert.equal(doc.activeElement, p.interrupt);
});

test('a later toast that replaces the one holding focus does not drop it', () => {
  const p = page();
  p.list.focus();
  toast('Archived.', true, { label: 'Undo', run: () => {} });
  p.toastEl.querySelector('button').focus();
  toast('Copied session ID', true);
  assert.equal(doc.activeElement, p.list);
});

// ---------- a dialog whose opener is gone ----------

test('a dialog that closes when its opener was removed returns focus to the last control outside dialogs, else the list', () => {
  const p = page();
  initDialogFocus(doc);
  const fire = (type, target) => { for (const f of docListeners[type] ?? []) f({ type, target }); };
  const opener = doc.createElement('button');
  p.view.append(opener);
  opener.focus(); fire('focusin', opener);
  const dlg = doc.createElement('dialog');
  doc.body.append(dlg);
  const ok = doc.createElement('button');
  dlg.append(ok);
  dlg.open = true;
  ok.focus(); fire('focusin', ok);
  assert.equal(doc.activeElement, ok);
  // Closing: the dialog is removed by its own handler; the opener is still there, so it gets focus.
  dlg.open = false; dlg.remove(); fire('close', dlg); flushTimers();
  assert.equal(doc.activeElement, opener);
  // The same again when the opener is gone as well.
  const dlg2 = doc.createElement('dialog');
  doc.body.append(dlg2);
  const ok2 = doc.createElement('button');
  dlg2.append(ok2); dlg2.open = true; ok2.focus(); fire('focusin', ok2);
  opener.remove();
  dlg2.open = false; dlg2.remove(); fire('close', dlg2); flushTimers();
  assert.equal(doc.activeElement, p.list);
  // A dialog that already returned focus is left alone.
  const dlg3 = doc.createElement('dialog');
  doc.body.append(dlg3);
  dlg3.open = true; dlg3.remove(); p.message.focus(); fire('close', dlg3); flushTimers();
  assert.equal(doc.activeElement, p.message);
});

// ---------- the to-do panel and wiring that needs a browser ----------

const src = (p) => readFileSync(here(`../src/${p}`), 'utf8');

test('the to-do panel keeps focus on Caught up / Hide across a refresh and lands on the list when Hide closes a column', () => {
  const todo = src('todo.ts');
  assert.match(todo, /keepFocus\(ui\.head, \(\) => ui!\.head\.replaceChildren\(/);
  assert.match(todo, /held && !panel\.open && focusStranded\(\)\) landFocus\(\['session-list'\]\)/);
});

test('the net and the shared helper are wired once, and no branch depends on the Vim keys setting', () => {
  assert.match(src('main.ts'), /initDialogFocus\(\);/);
  assert.match(src('sessions.ts'), /keepFocusIn\(\$\('session-view'\), drawHeader, HEADER_FOCUS_FALLBACKS\)/);
  assert.match(src('sessions.ts'), /renderSecondary\(\$\('s-secondary'\)/);
  for (const f of ['approvals.ts', 'session-secondary.ts', 'dialog-focus.ts', 'focus-return.ts']) {
    assert.doesNotMatch(src(f), /isVimKeysOn|vimkeys/, `${f} behaves the same with the Vim keys on or off`);
  }
});

// ---------- merged work: the runtime-error notice (#295) and a notification click (#294) ----------

const { reportRuntimeError, dismissRuntimeNotice } = await import('../dist/runtime-notice.js');
const { landOnOpenedSession } = await import('../dist/approvals.js');

test('the runtime-error notice never takes focus; Dismiss gives it back to the control Tab came from', () => {
  const p = page();
  p.message.focus();
  assert.equal(reportRuntimeError('TypeError: x is not a function (test-a)', () => {}, 1000), true);
  assert.equal(doc.activeElement, p.message, 'showing the notice does not move focus');
  const notice = byId.get('runtime-notice') ?? doc.body.querySelector('.runtime-notice');
  assert.ok(notice, 'the notice is on the page');
  const dismiss = buttonNamed(notice, 'Dismiss');
  // Tab into the notice from the message box.
  dismiss.focus();
  for (const f of notice.listeners.focusin ?? []) f({ type: 'focusin', target: dismiss, relatedTarget: p.message });
  dismiss.click();
  assert.equal(notice.hidden, true);
  assert.equal(doc.activeElement, p.message);
});

test('a dismissed notice whose opener is gone lands on the session list, not the page', () => {
  const p = page();
  p.list.focus();
  reportRuntimeError('TypeError: y is not a function (test-b)', () => {}, 2000);
  const notice = doc.body.querySelector('.runtime-notice');
  const dismiss = buttonNamed(notice, 'Dismiss');
  dismiss.focus();
  dismissRuntimeNotice();
  assert.equal(doc.activeElement, p.list);
});

test('a notification click opens the session on its waiting question, else the message box', () => {
  const p = page();
  initApprovals({ toast: () => {}, refresh: () => {}, selected: () => session() });
  const s = session({ pending_question: ask('Which one?') });
  renderApproval(s);
  landOnOpenedSession();
  assert.ok(p.banner.contains(doc.activeElement), 'focus is in the banner');
  renderApproval(session());
  landOnOpenedSession();
  assert.equal(doc.activeElement, p.message);
});
