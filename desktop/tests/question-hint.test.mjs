// The question dialog with Vim keys off (local #246): the hint names the keys and mouse actions that work
// (click or Space on an option, Up/Down, Enter for the next question), never the Vim ones; each option is a
// button in native Tab order that Space or a click picks; with Vim keys on the TUI text is unchanged. The fake
// DOM follows screen-reader.test.mjs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

/** One simple selector: tag, .class, [attr] or [attr="v"]; enough for TAB_STOP_SELECTOR and the modules here. */
function matchesSimple(n, sel) {
  const m = /^([a-z0-9]*)((?:\.[\w-]+)*)((?:\[[^\]]+\])*)$/i.exec(sel.trim());
  if (!m || n.tag.startsWith('#')) return false;
  if (m[1] && n.tag !== m[1].toLowerCase()) return false;
  for (const c of m[2].split('.').filter(Boolean)) if (!n.className.split(/\s+/).includes(c)) return false;
  for (const a of m[3].match(/\[[^\]]+\]/g) ?? []) {
    const [, name, val] = /^\[([\w-]+)(?:="([^"]*)")?\]$/.exec(a) ?? [];
    if (name === 'open') { if (!n.open) return false; continue; }
    const v = n.getAttribute(name);
    if (v === null || (val !== undefined && v !== val)) return false;
  }
  return true;
}
const matchesAny = (n, selector) => selector.split(',').some((s) => matchesSimple(n, s.trim().split(/\s+/).at(-1)));

class FakeEl {
  constructor(tag) {
    this.tag = tag; this.children = []; this.listeners = {}; this._text = ''; this.className = ''; this.attrs = {}; this.parent = null;
    this.dataset = {}; this.style = {}; this.hidden = false; this.disabled = false; this.value = ''; this.title = ''; this.open = false;
    const self = this;
    this.classList = {
      toggle(c, on) { const set = new Set(self.className.split(/\s+/).filter(Boolean)); if (on) set.add(c); else set.delete(c); self.className = [...set].join(' '); },
      add(c) { this.toggle(c, true); }, remove(c) { this.toggle(c, false); }, contains(c) { return self.className.split(/\s+/).includes(c); },
    };
  }
  // Reflected attributes, as in a real DOM: tabIndex, type, href, id.
  get tabIndex() { const v = this.attrs.tabindex; return v === undefined ? -1 : Number(v); }
  set tabIndex(v) { this.attrs.tabindex = String(v); }
  get type() { return this.attrs.type ?? (this.tag === 'input' ? 'text' : this.tag === 'button' ? 'submit' : ''); }
  set type(v) { this.attrs.type = String(v); }
  get href() { return this.attrs.href ?? ''; }
  set href(v) { this.attrs.href = String(v); }
  get id() { return this.attrs.id ?? ''; }
  set id(v) { this.attrs.id = String(v); }
  get textContent() { return this._text + this.children.map((c) => c.textContent).join(''); }
  set textContent(v) { this._text = String(v); this.children = []; }
  append(...kids) {
    for (const k of kids) {
      if (typeof k === 'string') { this.append(Object.assign(new FakeEl('#text'), { _text: k })); continue; }
      if (k.tag === '#frag') { this.append(...k.children); continue; }
      k.parent = this; this.children.push(k);
    }
  }
  prepend(...kids) { const rest = this.children; this.children = []; this.append(...kids); this.children.push(...rest); }
  replaceChildren(...kids) { this._text = ''; this.children = []; this.append(...kids); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this); this.parent = null; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  hasAttribute(k) { return k in this.attrs; }
  removeAttribute(k) { delete this.attrs[k]; }
  toggleAttribute(k, on) { if (on ?? !(k in this.attrs)) this.attrs[k] = ''; else delete this.attrs[k]; }
  matches(sel) { return matchesAny(this, sel); }
  querySelectorAll(sel) { return this.walk().slice(1).filter((n) => matchesAny(n, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] ?? null; }
  addEventListener(t, f) { (this.listeners[t] ??= []).push(f); }
  removeEventListener() {}
  fire(t, ev = {}) { const e = { target: this, currentTarget: this, preventDefault() { e.prevented = true; }, stopPropagation() {}, ...ev }; for (const f of this.listeners[t] ?? []) f(e); return e; }
  closest(sel) { for (let n = this; n; n = n.parent) if (!n.tag.startsWith('#') && matchesAny(n, sel)) return n; return null; }
  contains(n) { for (; n; n = n.parent) if (n === this) return true; return false; }
  focus() { doc.activeElement = this; }
  blur() {}
  select() {}
  scrollIntoView() {}
  walk(out = []) { out.push(this); for (const c of this.children) c.walk(out); return out; }
  showModal() { this.open = true; }
  close() { if (!this.open) return; this.open = false; this.onclose?.(); this.fire('close'); }
}

const body = new FakeEl('body');
const doc = {
  documentElement: new FakeEl('html'), title: '', activeElement: null, body, head: new FakeEl('head'),
  addEventListener() {}, removeEventListener() {},
  contains: () => true,
  querySelector: (sel) => body.querySelector(sel), querySelectorAll: (sel) => body.querySelectorAll(sel),
  getElementById: (id) => body.walk().find((n) => n.id === id) ?? null,
  createElement: (tag) => new FakeEl(tag),
  createDocumentFragment: () => new FakeEl('#frag'),
  createTextNode: (text) => Object.assign(new FakeEl('#text'), { _text: text }),
};
body.ownerDocument = doc;
doc.createTreeWalker = () => ({ nextNode: () => null });
for (const id of ['keyhint', 'cmdline', 'toast']) { const e = new FakeEl('div'); e.id = id; e.hidden = true; body.append(e); }
const vimBox = new FakeEl('input'); vimBox.id = 'vim-keys'; body.append(vimBox);
globalThis.MutationObserver = class { observe() {} };
globalThis.HTMLInputElement = class {};
globalThis.HTMLTextAreaElement = class {};
const realSetInterval = globalThis.setInterval;
globalThis.setInterval = (...a) => realSetInterval(...a).unref();
globalThis.window = globalThis;
const stored = new Map();
globalThis.localStorage = { getItem: (k) => stored.get(k) ?? null, setItem: (k, v) => stored.set(k, String(v)), removeItem: (k) => stored.delete(k) };
globalThis.addEventListener ??= () => {};
globalThis.document = doc;
Object.defineProperty(globalThis, 'navigator', { value: { platform: 'Linux x86_64', userAgent: 'test' }, configurable: true });

stored.set('rsi.desktop.vimKeysAsked', 'answered');
globalThis.__TAURI__ = { core: { invoke: async () => ({}) } };
const { initVimKeys, setVimKeys } = await import('../dist/vimkeys.js');
const lifecycle = await import('../dist/actions/lifecycle.js');
const { questionHint, openQuestion, questionKey } = await import('../dist/lifecycle-model.js');
const { formatKeyHint, stripKeyLabel } = await import('../dist/key-hints.js');
const { state } = await import('../dist/state.js');
initVimKeys();

const tick = () => new Promise((r) => setTimeout(r, 0));
const opt = (label) => ({ label, description: '' });
const single = { question: 'Which?', header: 'Pick', multiSelect: false, options: [opt('A'), opt('B'), opt('C')] };
const multi = { question: 'Which ones?', header: 'Many', multiSelect: true, options: [opt('X'), opt('Y')] };
const free = { question: 'Say something', header: 'Free' };

async function openWith(questions) {
  const now = '2026-10-20T12:00:00.000000000Z';
  state.sessions = [{
    id: '00000000-0000-4000-8000-000000000001', title: 'Waiting worker', status: 'WaitingApproval', provider: 'Claude',
    working_dir: '/tmp', created_at: now, updated_at: now, parent_id: null, kind: 'Session',
    pending_question: { questions },
  }];
  state.selectedId = state.sessions[0].id;
  lifecycle.openQuestionModal();
  await tick();
  const dlg = body.walk().find((n) => n.tag === 'dialog' && n.id === 'question-dialog');
  assert.ok(dlg?.open, 'the question dialog is open');
  return dlg;
}
const noteOf = (dlg) => dlg.querySelector('.dialog-note').textContent;
const optionButtons = (dlg) => dlg.querySelectorAll('.opt-btn');
const marks = (dlg) => dlg.querySelectorAll('.opt-mark').map((m) => m.textContent).join('');
// A real click on the button bubbles to its row, where the handler is.
const click = (b, detail = 0) => b.closest('li').fire('click', { detail });
const key = (dlg, k, extra = {}) => dlg.onkeydown({ key: k, target: doc.activeElement ?? dlg, ctrlKey: false, metaKey: false, shiftKey: false, altKey: false, isComposing: false, preventDefault() {}, ...extra });

test('Vim keys off: the hint says how to pick, move and go on, with no Vim key and no "next or previous"', async () => {
  setVimKeys(false);
  const dlg = await openWith([single, multi]);
  const text = noteOf(dlg);
  assert.match(text, /Click an option or press Space on it to pick it/);
  assert.match(text, /Up\/Down move between options/);
  assert.match(text, /Enter goes to the next question/);
  assert.match(text, /Ctrl\/Cmd-Enter submits|Ctrl\+Enter submits/);
  assert.doesNotMatch(text, /next or previous|j\/k|1-9|Backspace|\bd lets the agent|i type/);
  assert.equal(dlg.querySelector('.dialog-note').dataset.keyPlain, '', 'marked so the key-hint pass leaves it alone');
  dlg.close();
  const last = await openWith([single]);
  assert.doesNotMatch(noteOf(last), /Enter goes to the next question/, 'the last question has no next one');
  last.close();
});

test('Vim keys on: the hint is the TUI text and carries no plain mark', async () => {
  setVimKeys(true);
  const dlg = await openWith([single, multi]);
  assert.equal(noteOf(dlg), 'j/k move, Space or 1-9 choose, Enter/Backspace next or previous question, i type an answer, Ctrl/Cmd-Enter submits, d lets the agent decide, Esc hides.');
  assert.equal(dlg.querySelector('.dialog-note').dataset.keyPlain, undefined);
  dlg.close();
  const st = openQuestion([free]);
  assert.match(questionHint(st, false), /^Type an answer\./, 'a free-text question reads the same in both states');
});

test('the hint model: Vim-off sentence names no Vim key', () => {
  const st = openQuestion([single, multi]);
  const off = questionHint(st, false, 'mac');
  assert.match(off, /Cmd\+Enter submits/);
  assert.equal(questionHint(st, true), 'j/k move, Space or 1-9 choose, Enter/Backspace next or previous question, i type an answer, Ctrl/Cmd-Enter submits, d lets the agent decide, Esc hides.');
});

test('formatKeyHint never turns "Enter/Backspace next or previous question" into a sentence where Enter does both', () => {
  const tui = 'j/k move, Space or 1-9 choose, Enter/Backspace next or previous question, i type an answer, Ctrl/Cmd-Enter submits, d lets the agent decide, Esc hides.';
  const out = formatKeyHint(tui);
  assert.doesNotMatch(out, /Enter next or previous/);
  assert.doesNotMatch(out, /Enter\/Backspace/);
  assert.equal(formatKeyHint('Enter/Backspace next or previous question'), '');
  assert.equal(formatKeyHint('Enter/Backspace next or previous question · Esc hides'), 'Esc hides');
  assert.equal(formatKeyHint('j/k next or previous match'), '', 'a clause with only Vim keys still vanishes');
  assert.equal(stripKeyLabel('Next ▶'), 'Next ▶');
});

test('Vim keys off: each option is a button in Tab order and a click picks it', async () => {
  setVimKeys(false);
  const dlg = await openWith([single]);
  const btns = optionButtons(dlg);
  assert.equal(btns.length, 3);
  for (const b of btns) { assert.equal(b.tag, 'button'); assert.equal(b.getAttribute('type'), 'button'); assert.equal(b.getAttribute('tabindex') === '-1', false); assert.equal(b.getAttribute('role'), 'radio'); }
  assert.equal(marks(dlg), '○○○');
  btns[1].closest('li').fire('click', { detail: 1 });
  assert.equal(marks(dlg), '○●○');
  assert.equal(optionButtons(dlg)[1].getAttribute('aria-checked'), 'true');
  assert.equal(optionButtons(dlg)[0], btns[0], 'the rows are kept, so a focused option stays focused');
  dlg.close();
});

test('Vim keys off: Space on a focused option picks it (native button click) and a multi-select option toggles', async () => {
  setVimKeys(false);
  const dlg = await openWith([multi]);
  const btns = optionButtons(dlg);
  assert.equal(btns[0].getAttribute('role'), 'checkbox');
  // Space on a focused button is the browser's own click; the dialog's key handler stays out of it.
  btns[0].focus();
  key(dlg, ' ');
  assert.equal(marks(dlg), '☐☐', 'the dialog handler left Space to the focused button');
  click(btns[0]);
  click(btns[1]);
  assert.equal(marks(dlg), '☑☑');
  btns[0].closest('li').fire('click', { detail: 0 });
  assert.equal(marks(dlg), '☐☑');
  dlg.close();
});

test('Vim keys off: the arrows move the choice and carry focus to the option, so Space reaches it', async () => {
  setVimKeys(false);
  const dlg = await openWith([multi]);
  const btns = optionButtons(dlg);
  key(dlg, 'ArrowDown');
  assert.equal(doc.activeElement, btns[1], 'focus follows the cursor');
  assert.ok(btns[1].closest('li').classList.contains('sel'));
  key(dlg, 'ArrowUp');
  assert.equal(doc.activeElement, btns[0]);
  dlg.close();
  const one = await openWith([single]);
  key(one, 'ArrowDown');
  assert.equal(marks(one), '○●○', 'single-select picks as the cursor lands, as in the TUI');
  one.close();
});

test('Vim keys on: the keys are unchanged (j picks the next single-select option, Space picks the cursor, Enter goes on)', async () => {
  setVimKeys(true);
  const dlg = await openWith([single, multi]);
  key(dlg, 'j');
  assert.equal(marks(dlg), '○●○');
  key(dlg, 'k');
  key(dlg, ' ');
  assert.equal(marks(dlg), '●○○');
  assert.notEqual(doc.activeElement, optionButtons(dlg)[0], 'Vim keys on never moves focus onto an option');
  key(dlg, 'Enter');
  assert.match(dlg.querySelector('.approval-header').textContent, /Question 2 of 2/);
  assert.equal(marks(dlg), '☐☐');
  assert.equal(questionKey(openQuestion([single]), { key: '2' }).st.chosen[0][0], 1);
  dlg.close();
});

// ---- local #270: the skip button says what the agent receives ----

const approvals = await import('../dist/approvals.js');
const { DECLINE_RESPONSE, SKIP_LABEL, SKIP_NOTE, SKIP_TOAST } = await import('../dist/approvals-model.js');
const calls = [];
globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => { calls.push({ cmd, args }); return {}; } } };
const answers = () => calls.filter((c) => c.cmd === 'answer_question').map((c) => c.args.responseText);
const toasts = [];
// refreshSessions after a sent answer paints the connection pill; the fake daemon answers every call with {}.
const connPill = new FakeEl('span'); connPill.id = 'conn'; body.append(connPill);
const bannerHost = new FakeEl('div'); bannerHost.id = 'approval-banner'; body.append(bannerHost);
approvals.initApprovals({ toast: (m) => toasts.push(m), refresh() {}, selected: () => state.sessions[0] });
const pressed = (host, label) => host.querySelectorAll('button').find((b) => b.textContent.startsWith(label));

test('#270 banner: "Let the agent decide" is a neutral button that sends the TUI decline text and says so', async () => {
  const sess = (await openWith([single]), state.sessions[0]);
  body.walk().find((n) => n.id === 'question-dialog').close();
  approvals.renderApproval(sess);
  const labels = bannerHost.querySelectorAll('button').map((b) => b.textContent);
  assert.deepEqual(labels, ['Let the agent decide', 'Submit answer']);
  assert.ok(!labels.includes('Decline'), 'the old label is gone');
  const skip = pressed(bannerHost, 'Let the agent decide');
  assert.ok(!skip.classList.contains('danger'), 'not styled as a destructive red button');
  assert.match(skip.title, /no preference/);
  assert.match(skip.title, /not a "no"/);
  assert.equal(bannerHost.querySelector('.approval-note').textContent, SKIP_NOTE);
  assert.match(SKIP_NOTE, /To say no, pick an option or type your answer/);
  skip.fire('click');
  await tick();
  assert.deepEqual(answers().slice(-1), [DECLINE_RESPONSE]);
  assert.equal(DECLINE_RESPONSE, '(declined — no preference; use your best judgment)');
  assert.equal(toasts.at(-1), SKIP_TOAST);
});

test('#270 banner: picking an option and submitting still sends that option, not the skip text', async () => {
  const sess = (await openWith([single]), state.sessions[0]);
  body.walk().find((n) => n.id === 'question-dialog').close();
  approvals.renderApproval({ ...sess, id: 'other-id' });
  const radios = bannerHost.querySelectorAll('input');
  radios[1].checked = true;
  pressed(bannerHost, 'Submit answer').fire('click');
  await tick();
  assert.equal(answers().at(-1), 'B');
  assert.equal(toasts.at(-1), 'Answer sent');
});

test('#270 dialog: the same neutral label, tooltip and note; the keys still decline (Vim on) and the Vim-off hint stays accurate', async () => {
  setVimKeys(true);
  let dlg = await openWith([single]);
  const skip = pressed(dlg, 'Let the agent decide');
  assert.equal(skip.textContent, 'Let the agent decide (d)');
  assert.ok(!skip.classList.contains('danger'));
  assert.match(skip.title, /no preference/);
  assert.equal(dlg.querySelector('.approval-note').textContent, SKIP_NOTE);
  assert.match(noteOf(dlg), /d lets the agent decide/);
  assert.doesNotMatch(noteOf(dlg), /declines/);
  const before = answers().length;
  key(dlg, 'd'); // Vim on: the d key
  await tick();
  assert.equal(answers().length, before + 1);
  assert.equal(answers().at(-1), DECLINE_RESPONSE);
  assert.equal(toasts.length > 0, true);
  assert.equal(dlg.open, false, 'a sent answer closes the dialog');

  setVimKeys(false);
  dlg = await openWith([single]);
  assert.doesNotMatch(noteOf(dlg), /\bd\b.*(decide|declines)/, 'Vim off: the hint names no d key');
  assert.equal(key(dlg, 'd')?.prevented ?? false, false);
  pressed(dlg, 'Let the agent decide').fire('click');
  await tick();
  assert.equal(answers().at(-1), DECLINE_RESPONSE, 'the mouse path works with Vim keys off');
  assert.equal(SKIP_LABEL, 'Let the agent decide');
});
