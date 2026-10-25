// Keyboard focus return (local #244): closing a view, Esc in the message box, Enter on a button and the
// first-run cards never leave focus on the page body. The helper takes the document, so a fake one drives it.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  cardPrimaryButton, focusCardPrimary, focusList, keepFocus, openerOf, rememberOpener, returnFocus, shouldBlurAfterClick,
} from '../dist/focus-return.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

/** A tiny document: elements with a parent chain, `hidden`, and focus that only lands on visible, enabled ones. */
function world() {
  const doc = { activeElement: null, byId: new Map() };
  const body = mk('BODY');
  doc.body = body;
  doc.activeElement = body;
  doc.getElementById = (id) => doc.byId.get(id) ?? null;
  function mk(tag, opts = {}) {
    const e = {
      tagName: tag, id: opts.id ?? '', className: opts.className ?? '', textContent: opts.text ?? '', parent: opts.parent ?? null,
      hidden: false, disabled: opts.disabled ?? false, isConnected: true, children: [],
      focus() {
        if (this.disabled || !this.isConnected || this.closest('[hidden]')) return;
        doc.activeElement = this;
      },
      closest(sel) {
        for (let n = this; n; n = n.parent) if (sel === '[hidden]' && n.hidden) return n;
        return null;
      },
      contains(n) { for (let x = n; x; x = x.parent) if (x === this) return true; return false; },
      querySelectorAll(sel) { return this.children.filter((c) => c.tagName === 'BUTTON'); },
      querySelector(sel) {
        const ok = (c) => c.tagName === 'BUTTON' && !c.disabled && (!sel.startsWith('button.primary') || c.className.includes('primary'));
        return this.children.find(ok) ?? null;
      },
    };
    if (opts.parent) opts.parent.children.push(e);
    if (opts.id) doc.byId.set(opts.id, e);
    return e;
  }
  return { doc, mk, body };
}

test('closing a view returns focus to the element that opened it', () => {
  const { doc, mk } = world();
  const top = mk('BUTTON', { id: 'open-palette' });
  mk('UL', { id: 'session-list' });
  const view = mk('SECTION', { id: 'settings-view' });
  top.focus();
  rememberOpener('settings', doc);
  view.focus(); // the view takes focus when it opens
  returnFocus('settings', doc);
  assert.equal(doc.activeElement, top);
});

test('a view opened from the page body falls back to the session list, never the body', () => {
  const { doc, mk, body } = world();
  const list = mk('UL', { id: 'session-list' });
  const view = mk('SECTION', { id: 'dag-view' });
  assert.equal(doc.activeElement, body);
  rememberOpener('dag', doc);
  view.focus();
  returnFocus('dag', doc);
  assert.equal(doc.activeElement, list);
});

test('an opener that is gone, hidden or disabled falls back to the list', () => {
  const { doc, mk } = world();
  const list = mk('UL', { id: 'session-list' });
  for (const hide of [(b) => { b.isConnected = false; }, (b) => { b.parent.hidden = true; }, (b) => { b.disabled = true; }]) {
    const pane = mk('DIV');
    const b = mk('BUTTON', { parent: pane });
    b.focus();
    rememberOpener('graph-review', doc);
    hide(b);
    returnFocus('graph-review', doc);
    assert.equal(doc.activeElement, list);
    doc.activeElement = doc.body;
  }
});

test('the opener is forgotten once used, and a body opener erases an older one', () => {
  const { doc, mk, body } = world();
  const list = mk('UL', { id: 'session-list' });
  const a = mk('BUTTON');
  a.focus();
  rememberOpener('issues', doc);
  assert.equal(openerOf('issues'), a);
  returnFocus('issues', doc);
  assert.equal(openerOf('issues'), undefined);
  a.focus();
  rememberOpener('issues', doc);
  doc.activeElement = body;
  rememberOpener('issues', doc);
  returnFocus('issues', doc);
  assert.equal(doc.activeElement, list);
});

test('Esc in the message box moves focus to the session list', () => {
  const { doc, mk } = world();
  const list = mk('UL', { id: 'session-list' });
  const composer = mk('TEXTAREA', { id: 'message' });
  composer.focus();
  doc.activeElement = doc.body; // composer.blur()
  assert.equal(focusList('session-list', doc), list);
  assert.equal(doc.activeElement, list);
  assert.equal(focusList('missing', doc), null, 'no list: nothing throws');
});

test('Enter or Space on a button keeps focus; a pointer click, or Vim keys on, still gives it back', () => {
  assert.equal(shouldBlurAfterClick(0, false), false, 'keyboard click with Vim keys off');
  assert.equal(shouldBlurAfterClick(1, false), true, 'pointer click');
  assert.equal(shouldBlurAfterClick(0, true), true, 'Vim keys on: unchanged');
  assert.equal(shouldBlurAfterClick(1, true), true);
});

test('a redraw that recreates the focused button focuses the new one with the same label', () => {
  const { doc, mk, body } = world();
  const root = mk('DIV');
  mk('BUTTON', { parent: root, text: 'Runs', className: 'seg' });
  const saved = mk('BUTTON', { parent: root, text: 'Saved', className: 'seg' });
  saved.focus();
  keepFocus(root, () => {
    root.children.length = 0;
    saved.isConnected = false;
    doc.activeElement = body;
    mk('BUTTON', { parent: root, text: 'Runs', className: 'seg' });
    mk('BUTTON', { parent: root, text: 'Saved', className: 'seg' });
  }, doc);
  assert.equal(doc.activeElement.textContent, 'Saved');
  assert.notEqual(doc.activeElement, saved);
  assert.ok(root.contains(doc.activeElement));
});

test('a redraw leaves focus alone when the focused control was not a rebuilt button', () => {
  const { doc, mk } = world();
  const root = mk('DIV');
  mk('BUTTON', { parent: root, text: 'Runs' });
  const field = mk('INPUT');
  field.focus();
  keepFocus(root, () => {}, doc);
  assert.equal(doc.activeElement, field);
});

test('the daemon-down and first-run cards focus their main button', () => {
  const { doc, mk } = world();
  const card = mk('DIV');
  mk('BUTTON', { parent: card, text: 'Start rsid', className: 'btn primary', disabled: true });
  mk('BUTTON', { parent: card, text: 'Retry now', className: 'btn' });
  const create = mk('BUTTON', { parent: card, text: 'Create your first project', className: 'btn primary' });
  assert.equal(cardPrimaryButton(card), create, 'the first enabled primary button');
  assert.equal(focusCardPrimary(card, doc), create);
  assert.equal(doc.activeElement, create);
});

test('a card never steals focus from a field the user is typing in', () => {
  const { doc, mk } = world();
  const card = mk('DIV');
  mk('BUTTON', { parent: card, text: 'Start rsid', className: 'btn primary' });
  const filter = mk('INPUT');
  filter.focus();
  assert.equal(focusCardPrimary(card, doc), null);
  assert.equal(doc.activeElement, filter);
});

test('the views call the helper: every close returns focus, Esc in the box and the cards use it', () => {
  const src = (f) => readFileSync(here(`../src/${f}`), 'utf8');
  for (const [file, view] of [['settings.ts', 'settings'], ['dag.ts', 'dag'], ['graphs.ts', 'graph-review'], ['issues.ts', 'issues'], ['managers.ts', 'managers'], ['dashboard.ts', 'dashboard']]) {
    assert.match(src(file), new RegExp(`rememberOpener\\('${view}'\\)`), `${file} remembers its opener`);
    assert.match(src(file), new RegExp(`returnFocus\\('${view}'\\)`), `${file} returns focus on close`);
  }
  assert.match(src('actions/detail.ts'), /composer\.blur\(\); focusList\(\)/);
  assert.match(src('boot.ts'), /focusCardPrimary\(host\)/);
  for (const file of ['issues.ts', 'graphs.ts', 'dag.ts']) assert.match(src(file), /shouldBlurAfterClick\(/, `${file} keeps focus on a keyboard click`);
  assert.doesNotMatch(src('dag.ts') + src('graphs.ts') + src('issues.ts'), /window\.setTimeout\(\(\) => \w+\.blur\(\), 0\)\); *\}\)/, 'no unconditional blur after a click');
});
