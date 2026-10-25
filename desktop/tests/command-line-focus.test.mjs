// Local #280: a command line typed while a button or checkbox has focus. A mouse click on Show completed leaves the
// checkbox focused; `:term` + Enter then used to do nothing, because the window-level activation guard handed Enter
// to the checkbox before the key engine saw it. The key engine owns Enter and Space while its command line is open.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { KeyEngine, routeKey } from '../dist/keyengine.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { activationGuard, bindCommandLineOpen, focusOwnsKey } from '../dist/modal-gate.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const checkbox = () => {
  const el = { tagName: 'INPUT', type: 'checkbox', clicks: 0, click() { this.clicks++; } };
  el.closest = (sel) => (sel.split(',').map((x) => x.trim()).includes('input[type="checkbox"]') ? el : null);
  return el;
};
const button = () => {
  const el = { tagName: 'BUTTON', clicks: 0, click() { this.clicks++; } };
  el.closest = (sel) => (sel.split(',').map((x) => x.trim()).includes('button') ? el : null);
  return el;
};

/** One keydown the way the page sees it: the window capture guard first, then vimkeys' document handler. */
function press(engine, key, target) {
  const calls = [];
  const ev = { key, code: /^[a-z]$/.test(key) ? `Key${key.toUpperCase()}` : '', target, stopImmediatePropagation: () => calls.push('stop'), preventDefault: () => calls.push('prevent') };
  activationGuard(ev);
  if (calls.includes('stop') || focusOwnsKey(key, target)) return { reached: false, calls };
  return { reached: true, calls, outcome: routeKey(engine, ev, { platform: 'other', target, modalOpen: false, now: 0 }) };
}

for (const [name, make] of [['checkbox', checkbox], ['button', button]]) {
  test(`:term then Enter runs the command with a ${name} focused (the Show completed click)`, () => {
    const engine = new KeyEngine(desktopKeymap());
    bindCommandLineOpen(() => engine.commandLineOpen());
    const target = make();
    let last;
    for (const k of ':term') last = press(engine, k, target);
    assert.equal(engine.commandLineOpen(), true);
    assert.equal(last.reached, true);
    const enter = press(engine, 'Enter', target);
    assert.equal(enter.reached, true, 'Enter reaches the key engine, not the focused control');
    assert.equal(enter.outcome.kind, 'run');
    assert.equal(engine.commandLineOpen(), false);
  });
}

test('Space inside a command line is part of the line and does not toggle the focused checkbox', () => {
  const engine = new KeyEngine(desktopKeymap());
  bindCommandLineOpen(() => engine.commandLineOpen());
  const target = checkbox();
  for (const k of ':term') press(engine, k, target);
  const space = press(engine, ' ', target);
  assert.equal(space.reached, true);
  assert.equal(space.outcome.kind, 'command');
});

test('with no command line open the focused control still owns Enter and Space (hub #3112)', () => {
  const engine = new KeyEngine(desktopKeymap());
  bindCommandLineOpen(() => engine.commandLineOpen());
  for (const target of [checkbox(), button()]) {
    for (const k of ['Enter', ' ']) {
      const r = press(engine, k, target);
      assert.equal(r.reached, false, `${target.tagName} ${JSON.stringify(k)}`);
      assert.deepEqual(r.calls, ['stop']);
    }
  }
  // A pending chord alone is not a command line: Enter still activates the button.
  press(engine, 'g', document_body());
  assert.equal(engine.busy(), true);
  assert.equal(press(engine, 'Enter', button()).reached, false);
  // Escape closes the command line, and the control owns Enter again.
  const target = button();
  engine.reset();
  press(engine, ':', target);
  assert.equal(engine.commandLineOpen(), true);
  press(engine, 'Escape', target);
  assert.equal(engine.commandLineOpen(), false);
  assert.equal(press(engine, 'Enter', target).reached, false);
});

function document_body() { return { tagName: 'BODY', closest: () => null }; }

test('initVimKeys binds the command-line check, and the smoke step filter-prompt no longer reloads the page', () => {
  assert.match(readFileSync(here('../src/vimkeys.ts'), 'utf8'), /bindCommandLineOpen\(\(\) => engine\.commandLineOpen\(\)\)/);
  const smoke = readFileSync(here('../scripts/smoke.mjs'), 'utf8');
  const step = smoke.slice(smoke.indexOf("id: 'filter-prompt'"), smoke.indexOf("id: 'terminal'"));
  assert.doesNotMatch(step, /p\.goto\(/, 'the reload that hid #280 stays out');
});
