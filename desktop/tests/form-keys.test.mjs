// The generic form's chords (local #338): the TUI's "Provider Form" table lists Ctrl-Enter (Save) and Ctrl-Q (Close).
// The desktop's custom-provider form is the generic openForm, so every form built on it takes both from any field:
// Ctrl/Cmd-Enter saves, Ctrl-Q closes without saving. A plain Enter and Esc stay the form's own submit and cancel.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FakeEl, mount } from './fake-doc.mjs';

mount('div', 'toast');
// The fake form has no requestSubmit: it fires the form's submit listeners, as the browser does.
FakeEl.prototype.requestSubmit = function requestSubmit() { this.fire('submit'); };

const { openForm } = await import('../dist/pickers.js');
const { formChord } = await import('../dist/overlay-keys.js');

const tick = () => new Promise((r) => setTimeout(r, 0));

function open() {
  const saved = [];
  const dlg = openForm({
    title: 'New custom provider', submit: 'Save',
    fields: [
      { name: 'name', label: 'Name', value: 'Fake gateway', type: 'text' },
      { name: 'on', label: 'Enabled', value: 'true', type: 'checkbox' },
    ],
    validate: () => null,
    onSubmit: async (values) => { saved.push(values); },
  });
  const form = dlg.querySelector('form');
  const [name, box] = dlg.querySelectorAll('input');
  return { dlg, form, name, box, saved, cancel: dlg.querySelector('.btn') };
}
// A keydown bubbles from the focused element up through the form to the dialog, as in the browser.
function press(target, key, more = {}) {
  const ev = { target, key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, ...more, preventDefault() { ev.prevented = true; }, stopPropagation() { ev.stopped = true; } };
  for (let n = target; n && !ev.stopped; n = n.parent) for (const f of n.listeners.keydown ?? []) f(ev);
  return ev;
}

test('formChord: Ctrl/Cmd-Enter saves, Ctrl-Q closes, nothing else is a chord', () => {
  assert.equal(formChord({ key: 'Enter', ctrlKey: true }), 'save');
  assert.equal(formChord({ key: 'Enter', metaKey: true }), 'save');
  assert.equal(formChord({ key: 'q', ctrlKey: true }), 'close');
  assert.equal(formChord({ key: 'Q', ctrlKey: true }), 'close');
  assert.equal(formChord({ key: 'Enter' }), null, 'a plain Enter is the form’s own submit');
  assert.equal(formChord({ key: 'q' }), null, 'a plain q is text');
  assert.equal(formChord({ key: 'q', metaKey: true }), null, 'Cmd-Q is the system Quit');
  assert.equal(formChord({ key: 'Escape', ctrlKey: true }), null);
  assert.equal(formChord({ key: 'Enter', ctrlKey: true, isComposing: true }), null, 'an IME confirm is not Save');
  assert.equal(formChord({ key: 'q', ctrlKey: true, altKey: true }), null);
});

test('Ctrl-Enter saves the form from a text field and from a checkbox', async () => {
  for (const pick of ['name', 'box']) {
    const o = open();
    const ev = press(o[pick], 'Enter', { ctrlKey: true });
    await tick();
    assert.equal(ev.prevented, true);
    assert.equal(o.saved.length, 1, `Ctrl-Enter from ${pick} saved once`);
    assert.deepEqual(o.saved[0], { name: 'Fake gateway', on: 'true' });
    assert.equal(o.dlg.open, false, 'the saved form closes');
  }
});

test('Cmd-Enter saves too (macOS, like the entity form)', async () => {
  const o = open();
  press(o.name, 'Enter', { metaKey: true });
  await tick();
  assert.equal(o.saved.length, 1);
});

test('Ctrl-Q closes the form without saving, from any field', async () => {
  for (const pick of ['name', 'box', 'cancel']) {
    const o = open();
    const ev = press(o[pick], 'q', { ctrlKey: true });
    await tick();
    assert.equal(ev.prevented, true);
    assert.equal(o.dlg.open, false, `Ctrl-Q from ${pick} closed the form`);
    assert.equal(o.saved.length, 0, 'nothing was saved');
  }
});

test('Ctrl-Enter on a focused button leaves the key to that button (Cancel never saves)', async () => {
  const o = open();
  const ev = press(o.cancel, 'Enter', { ctrlKey: true });
  await tick();
  assert.equal(ev.prevented, undefined);
  assert.equal(o.saved.length, 0);
  assert.equal(o.dlg.open, true);
});

test('plain keys are untouched: q types, Enter is the native submit, so neither closes the form here', async () => {
  const o = open();
  const q = press(o.name, 'q');
  const enter = press(o.name, 'Enter');
  await tick();
  assert.equal(q.prevented, undefined);
  assert.equal(enter.prevented, undefined);
  assert.equal(o.dlg.open, true);
  assert.equal(o.saved.length, 0);
});

test('a validation failure keeps the form open on Ctrl-Enter, like the Save button', async () => {
  const dlg = openForm({
    title: 'T', submit: 'Save', fields: [{ name: 'name', label: 'Name', value: '', type: 'text' }],
    validate: (v) => (v.name ? null : 'Name is required'), onSubmit: async () => { throw new Error('unreachable'); },
  });
  press(dlg.querySelector('input'), 'Enter', { ctrlKey: true });
  await tick();
  assert.equal(dlg.open, true);
  assert.match(dlg.querySelector('.form-error').textContent, /Name is required/);
});
