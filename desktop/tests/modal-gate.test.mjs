// The Vim keys switch reaches every pane key handler (hub #3101).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { activationGuard, dialogKeyBlocked, focusOwnsKey, installDialogGate, paneKeyAllowed, STANDARD_KEYS } from '../dist/modal-gate.js';
import { INITIAL_NAV, paneKey } from '../dist/settings-model.js';

const keydown = (key) => Object.assign(new Event('keydown', { cancelable: true }), { key });

test('with Vim keys off the Settings pane keeps standard keys and drops its modal letters', () => {
  const items = { ...INITIAL_NAV, focus: 'items' };
  // The keys the hub repro presses: w applies, q closes (discarding pending edits).
  assert.equal(paneKey(items, 'w', false, 3).kind, 'apply');
  assert.equal(paneKey(items, 'q', false, 3).kind, 'close');
  for (const key of ['w', 'q', 'j', 'k', 'R', '/', 'n', 'x']) {
    assert.equal(paneKeyAllowed(true, keydown(key)), true, `on: ${key}`);
    assert.equal(paneKeyAllowed(false, keydown(key)), false, `off: ${key}`);
  }
  for (const key of ['ArrowDown', 'ArrowUp', 'Enter', 'Escape', 'Tab', 'PageDown', 'Home']) {
    assert.equal(paneKeyAllowed(false, keydown(key)), true, `off keeps ${key}`);
    assert.equal(paneKeyAllowed(true, keydown(key)), true, `on keeps ${key}`);
  }
  assert.equal(paneKey(items, 'ArrowDown', false, 3).kind, paneKey(items, 'j', false, 3).kind);
  assert.equal(STANDARD_KEYS.has('w'), false);
});

test('every pane window keydown handler asks paneKeyAllowed first', () => {
  const src = fileURLToPath(new URL('../src/', import.meta.url));
  // Not pane handlers: the key engine itself, the palette chord (works with Vim keys off by
  // design) and the prompt editors, which consult isVimKeysOn() for their own normal mode.
  const exempt = new Set(['vimkeys.ts', 'palette.ts', 'prompts.ts', 'prompt-creator.ts']);
  const gated = [];
  for (const file of readdirSync(src).filter((f) => f.endsWith('.ts')).sort()) {
    const text = readFileSync(src + file, 'utf8');
    const at = text.indexOf("window.addEventListener('keydown', (ev) => {");
    if (at < 0 || exempt.has(file)) continue;
    const head = text.slice(at, at + 400).split('\n').slice(1, 4).join('\n');
    assert.match(head, /if \(!paneKeyAllowed\(isVimKeysOn\(\), ev\)\) return;/, `${file} pane handler ignores the Vim keys switch`);
    gated.push(file);
  }
  assert.deepEqual(gated, ['dag.ts', 'esp.ts', 'files.ts', 'graphs.ts', 'issues.ts', 'managers.ts', 'settings.ts']);
});

// A stand-in for a DOM element: closest() answers for the selectors it is inside of.
const inside = (...tags) => ({ closest: (sel) => (sel.split(',').map((x) => x.trim().split('[')[0]).some((t) => tags.includes(t)) ? {} : null) });

test('Enter and Space on a focused button, link or summary stay with that control (hub #3112)', () => {
  for (const key of ['Enter', ' ']) {
    assert.equal(focusOwnsKey(key, inside('button')), true, `button ${JSON.stringify(key)}`);
    assert.equal(focusOwnsKey(key, inside('a')), true);
    assert.equal(focusOwnsKey(key, inside('summary')), true);
    // The dialog itself (a row list) keeps its row action.
    assert.equal(focusOwnsKey(key, inside('dialog')), false);
    assert.equal(focusOwnsKey(key, null), false);
  }
  for (const key of ['j', 'ArrowDown', 'Escape', 'e']) assert.equal(focusOwnsKey(key, inside('button')), false, key);
});

// The authority for Enter/Space on a focused control is the window capture guard
// (hub #3128); the per-handler focusOwnsKey lines are defence in depth only. The
// mounted proof (every registration style, the control still clicks) is the smoke
// harness step activation-guard-registrations.
const fakeEvent = (key, target) => {
  const calls = [];
  return { key, target, calls, stopImmediatePropagation: () => calls.push('stop'), preventDefault: () => calls.push('prevent') };
};
const element = (tagName, matches) => {
  const el = { tagName, clicks: 0, click() { this.clicks++; } };
  el.closest = (sel) => (sel.split(',').map((x) => x.trim()).some((x) => matches(x)) ? el : null);
  return el;
};

test('the activation guard stops every later listener for Enter/Space on a control and leaves native activation alone', () => {
  const button = element('BUTTON', (x) => x === 'button');
  const checkbox = element('INPUT', (x) => x === 'input[type="checkbox"]');
  const link = element('A', (x) => x === 'a[href]');
  link.hasAttribute = (name) => name === 'href';
  for (const key of ['Enter', ' ']) {
    for (const el of [button, checkbox, link]) {
      const ev = fakeEvent(key, el);
      assert.equal(activationGuard(ev), 'native');
      assert.deepEqual(ev.calls, ['stop'], `${el.tagName} ${JSON.stringify(key)}: no preventDefault, so the browser activates it`);
    }
  }
  // A non-native [role=button] has no browser activation: the guard clicks it.
  const twisty = element('SPAN', (x) => x === '[role="button"]');
  const ev = fakeEvent(' ', twisty);
  assert.equal(activationGuard(ev), 'emulated');
  assert.deepEqual(ev.calls, ['stop', 'prevent']);
  assert.equal(twisty.clicks, 1);
  // Hub #3158: an <a role=button> without href has no native activation either.
  for (const key of ['Enter', ' ']) {
    const anchor = element('A', (x) => x === '[role="button"]');
    anchor.hasAttribute = () => false;
    const e = fakeEvent(key, anchor);
    assert.equal(activationGuard(e), 'emulated');
    assert.deepEqual(e.calls, ['stop', 'prevent']);
    assert.equal(anchor.clicks, 1);
  }
  // Text fields, the dialog itself and other keys pass untouched.
  for (const [key, target] of [['Enter', element('INPUT', (x) => x === 'input')], [' ', element('DIALOG', () => false)], ['j', button], ['Escape', button]]) {
    const e = fakeEvent(key, target);
    assert.equal(activationGuard(e), null);
    assert.deepEqual(e.calls, []);
  }
});

test('main.ts installs the activation guard before any other module can register a listener', () => {
  const main = readFileSync(fileURLToPath(new URL('../src/main.ts', import.meta.url)), 'utf8');
  const firstImport = main.split('\n').find((l) => l.startsWith('import '));
  assert.equal(firstImport, "import './activation-guard.js';");
  const guard = readFileSync(fileURLToPath(new URL('../src/activation-guard.ts', import.meta.url)), 'utf8');
  assert.match(guard, /^installActivationGuard\(window\);$/m);
});

// ---- overlay dialogs (hub #3101 follow-up, local #111) ----
// One structural gate (window, capture, second import after the activation guard) instead of a
// call in every dialog handler. The mounted proof is the smoke step dialog-vim-gate.
const dialogEl = { tagName: 'DIALOG', closest: (sel) => (sel.includes('dialog') ? dialogEl : null) };
const buttonInDialog = { tagName: 'BUTTON', closest: (sel) => (sel.includes('dialog') ? dialogEl : null) };
const fieldInDialog = { tagName: 'INPUT', type: 'text', closest: (sel) => (sel.includes('dialog') ? dialogEl : null) };
const textareaInDialog = { tagName: 'TEXTAREA', closest: (sel) => (sel.includes('dialog') ? dialogEl : null) };
const body = { tagName: 'BODY', closest: () => null };
const keyOn = (target, key, extra = {}) => ({ key, target, ...extra });

test('with Vim keys off a dialog drops letters and keeps Enter, Escape, arrows, Tab and paging', () => {
  for (const key of ['a', 'd', 'j', 'k', 'q', 'R', 'y', 'n', 'e', 'r', ' ', 'H', '/']) {
    assert.equal(dialogKeyBlocked(false, keyOn(dialogEl, key)), true, `off: ${JSON.stringify(key)}`);
    assert.equal(dialogKeyBlocked(true, keyOn(dialogEl, key)), false, `on: ${JSON.stringify(key)}`);
    assert.equal(dialogKeyBlocked(false, keyOn(buttonInDialog, key)), true, `off, focused button: ${JSON.stringify(key)}`);
  }
  for (const key of STANDARD_KEYS) assert.equal(dialogKeyBlocked(false, keyOn(dialogEl, key)), false, `off keeps ${key}`);
  // Typing into a field, chords and IME composition are not letter keys.
  for (const field of [fieldInDialog, textareaInDialog, { tagName: 'SELECT', closest: dialogEl.closest }, { tagName: 'DIV', isContentEditable: true, closest: dialogEl.closest }]) {
    assert.equal(dialogKeyBlocked(false, keyOn(field, 'q')), false, `typing in ${field.tagName}`);
  }
  assert.equal(dialogKeyBlocked(false, keyOn(dialogEl, 'k', { ctrlKey: true })), false);
  assert.equal(dialogKeyBlocked(false, keyOn(dialogEl, 'k', { metaKey: true })), false);
  assert.equal(dialogKeyBlocked(false, keyOn(dialogEl, 'a', { isComposing: true })), false);
  // A checkbox is a control, not a text field: a letter on it is blocked.
  assert.equal(dialogKeyBlocked(false, keyOn({ tagName: 'INPUT', type: 'checkbox', closest: dialogEl.closest }, 'x')), true);
  // Outside any dialog the pane rule applies, not this gate; with a dialog open and focus on the body it blocks.
  assert.equal(dialogKeyBlocked(false, keyOn(body, 'j')), false);
  assert.equal(dialogKeyBlocked(false, keyOn(body, 'j'), true), true);
  assert.equal(dialogKeyBlocked(false, keyOn(null, 'j'), true), true);
  assert.equal(dialogKeyBlocked(false, keyOn(body, 'Enter'), true), false);
});

// A window with the gate installed, then a dialog handler registered afterwards the way every
// overlay does (the listener sits on the dialog; here a later window listener stands for it).
function dialogWindow(getVim, { dialogOpen = true } = {}) {
  const win = new EventTarget();
  win.document = { querySelector: () => (dialogOpen ? {} : null) };
  installDialogGate(win, getVim);
  return win;
}
const press = (win, target, key) => {
  const ev = new Event('keydown', { cancelable: true });
  Object.defineProperties(ev, { key: { value: key }, target: { value: target } });
  win.dispatchEvent(ev);
};

test('the budgets dialog keys (a/d/R/q, j/k) act with Vim keys on, only Enter and arrows with them off', () => {
  // The switch of budgets.ts, reduced to what it calls.
  for (const [vim, expected] of [[true, ['edit', 'new', 'delete', 'refresh', 'close', 'down', 'up']], [false, ['edit', 'down', 'up']]]) {
    const win = dialogWindow(() => vim);
    const seen = [];
    win.addEventListener('keydown', (ev) => {
      switch (ev.key) {
        case 'j': case 'ArrowDown': seen.push('down'); break;
        case 'k': case 'ArrowUp': seen.push('up'); break;
        case 'Enter': seen.push('edit'); break;
        case 'a': seen.push('new'); break;
        case 'd': seen.push('delete'); break;
        case 'R': seen.push('refresh'); break;
        case 'q': seen.push('close'); break;
        default:
      }
    });
    for (const key of ['Enter', 'a', 'd', 'R', 'q']) press(win, dialogEl, key);
    press(win, dialogEl, vim ? 'j' : 'ArrowDown');
    press(win, dialogEl, vim ? 'k' : 'ArrowUp');
    assert.deepEqual(seen, expected, `vim ${vim ? 'on' : 'off'}`);
  }
});

test('the managers confirm dialog: y and n act only with Vim keys on; Enter on a button and Escape stay', () => {
  for (const vim of [true, false]) {
    const win = dialogWindow(() => vim);
    const seen = [];
    win.addEventListener('keydown', (ev) => {
      if (ev.key === 'y') seen.push('accept'); else if (ev.key === 'n') seen.push('cancel');
    });
    press(win, dialogEl, 'y'); press(win, dialogEl, 'n');
    assert.deepEqual(seen, vim ? ['accept', 'cancel'] : [], `vim ${vim ? 'on' : 'off'}`);
  }
});

test('the gate reads the Vim keys switch per key and passes everything while no dialog is aimed at', () => {
  let vim = true;
  const win = dialogWindow(() => vim, { dialogOpen: false });
  const seen = [];
  win.addEventListener('keydown', (ev) => seen.push(ev.key));
  press(win, dialogEl, 'a'); vim = false; press(win, dialogEl, 'b'); press(win, body, 'c'); press(win, dialogEl, 'ArrowDown');
  assert.deepEqual(seen, ['a', 'c', 'ArrowDown']);
});

test('main.ts imports the dialog gate second and vimkeys.ts binds it to the Vim keys switch', () => {
  const main = readFileSync(fileURLToPath(new URL('../src/main.ts', import.meta.url)), 'utf8');
  const imports = main.split('\n').filter((l) => l.startsWith('import '));
  assert.deepEqual(imports.slice(0, 2), ["import './activation-guard.js';", "import './dialog-gate.js';"]);
  const gate = readFileSync(fileURLToPath(new URL('../src/dialog-gate.ts', import.meta.url)), 'utf8');
  assert.match(gate, /^installDialogGate\(window, \(\) => vimKeysOn\(\)\);$/m);
  const vim = readFileSync(fileURLToPath(new URL('../src/vimkeys.ts', import.meta.url)), 'utf8');
  assert.match(vim, /bindDialogGate\(isVimKeysOn\);/);
});

test('the dialog keydown handlers of the overlays all ride on the one gate', () => {
  const src = fileURLToPath(new URL('../src/', import.meta.url));
  const owners = [];
  for (const file of readdirSync(src, { recursive: true }).filter((f) => f.endsWith('.ts')).sort()) {
    const text = readFileSync(src + file, 'utf8');
    if (/(?:dlg|d|dialog)\.(?:onkeydown =|addEventListener\('keydown')/.test(text)) owners.push(file);
  }
  for (const file of ['budgets.ts', 'managers.ts', 'pickers.ts', 'admin-ops.ts', 'admin-keys.ts', 'admin-schedules.ts', 'assist.ts', 'skills.ts', 'todo.ts', 'themes.ts']) {
    assert.ok(owners.includes(file), `${file} has a dialog keydown handler and rides on the gate`);
  }
});
