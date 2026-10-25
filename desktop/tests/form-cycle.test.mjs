// The generic form (openForm: hooks, ...) steps a cycling field on Up / Down, wrapping (#337); typing still works. The Ctrl-Enter and Ctrl-Q
// chords (#338) and the formChord unit are in form-keys.test.mjs. Runs against the fake document (no browser).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import './fake-doc.mjs';

const { openForm } = await import('../dist/pickers.js');
const { HOOK_EVENTS } = await import('../dist/operator-config-model.js');

function form(over = {}) {
  const saved = [];
  const dlg = openForm({
    title: 'Add a Claude hook', submit: 'Add',
    fields: [
      { name: 'event', label: 'Event', value: 'PreToolUse', type: 'text', cycle: HOOK_EVENTS },
      { name: 'command', label: 'Command', value: 'ls', type: 'text' },
    ],
    validate: () => null,
    onSubmit: async (v) => { saved.push(v); },
    ...over,
  });
  const inputs = dlg.querySelectorAll('input');
  return { dlg, saved, event: inputs[0], command: inputs[1], form: dlg.querySelector('form') };
}
const press = (el, key, extra = {}) => el.fire('keydown', { key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, isComposing: false, ...extra });

test('Up and Down step the hook event through the nine events, wrapping, and typing still works (#337)', () => {
  assert.equal(HOOK_EVENTS.length, 9);
  const { event } = form();
  assert.equal(event.value, 'PreToolUse');
  assert.equal(press(event, 'ArrowDown').prevented, true);
  assert.equal(event.value, 'PostToolUse');
  press(event, 'ArrowDown');
  assert.equal(event.value, HOOK_EVENTS[2]);
  press(event, 'ArrowUp');
  press(event, 'ArrowUp');
  assert.equal(event.value, 'PreToolUse');
  press(event, 'ArrowUp');
  assert.equal(event.value, HOOK_EVENTS.at(-1), 'Up wraps from the first to the last');
  press(event, 'ArrowDown');
  assert.equal(event.value, 'PreToolUse', 'Down wraps from the last to the first');
  event.value = 'Sta';
  assert.equal(press(event, 'a').prevented, undefined, 'a typed letter is left to the box');
  assert.equal(event.value, 'Sta');
  press(event, 'ArrowDown');
  assert.equal(event.value, 'PreToolUse', 'a typed value that is no event starts at the first');
  event.value = 'Sta';
  press(event, 'ArrowUp');
  assert.equal(event.value, HOOK_EVENTS.at(-1), 'Up from a typed value starts at the last');
});

test('a field without cycle keeps Up and Down for the box', () => {
  const { command } = form();
  assert.equal(press(command, 'ArrowDown').prevented, undefined);
  assert.equal(command.value, 'ls');
});
