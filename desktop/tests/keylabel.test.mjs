// Issue #214: one platform-aware key-label helper and a visible Help button.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { chordLabel, hintLabel, modifierName, proseLabel, sendChord, sendPlaceholder } from '../dist/keylabel.js';
import { displayHint, keyHints, actionSpec } from '../dist/registry.js';
import { htmlButtonActions } from '../scripts/reachability-lib.mjs';

const html = readFileSync(new URL('../static/index.html', import.meta.url), 'utf8');

test('modifier names: Cmd/Option on macOS, Ctrl/Alt elsewhere', () => {
  assert.equal(modifierName('ctrl', 'mac'), 'Cmd');
  assert.equal(modifierName('alt', 'mac'), 'Option');
  assert.equal(modifierName('ctrl', 'other'), 'Ctrl');
  assert.equal(modifierName('alt', 'other'), 'Alt');
  assert.equal(modifierName('shift', 'mac'), 'Shift');
  assert.equal(chordLabel(['ctrl', 'alt'], 'G', 'mac'), 'Cmd+Option+G');
  assert.equal(chordLabel(['ctrl', 'alt'], 'G', 'other', '-'), 'Ctrl-Alt-G');
});

test('hint labels follow the platform and registry.displayHint uses them', () => {
  assert.equal(hintLabel('Ctrl-Alt-G', 'mac'), 'Cmd-Option-G');
  assert.equal(hintLabel('Ctrl-Alt-G', 'other'), 'Ctrl-Alt-G');
  assert.equal(hintLabel('Ctrl-N', 'mac'), 'Cmd-N');
  assert.equal(hintLabel(':help', 'mac'), ':help');
  assert.equal(displayHint('Ctrl-Alt-G', 'mac'), 'Cmd-Option-G');
  assert.equal(proseLabel('Ctrl+Enter or Alt+S', 'mac'), 'Cmd+Enter or Option+S');
  assert.equal(proseLabel('Ctrl+Enter or Alt+S', 'other'), 'Ctrl+Enter or Alt+S');
});

test('the message box send hint says Cmd+Enter on macOS and Ctrl+Enter elsewhere', () => {
  assert.equal(sendChord('mac'), 'Cmd+Enter');
  assert.equal(sendPlaceholder('mac'), 'Message this session (Cmd+Enter to send)');
  assert.equal(sendPlaceholder('other'), 'Message this session (Ctrl+Enter to send)');
});

test('a visible Help button runs the help action; ? and Ctrl-Alt-G still do', () => {
  assert.ok(htmlButtonActions(html).includes('help'), 'a toolbar button has data-action="help"');
  const btn = html.match(/<button[^>]*data-action="help"[^>]*>/)[0];
  assert.match(btn, /aria-label="Keyboard shortcuts and help"/);
  assert.match(btn, /title="[^"]*[Hh]elp/);
  assert.equal(actionSpec('help').title, 'Keyboard shortcuts');
  const keys = keyHints('help');
  assert.ok(keys.includes('?'));
  assert.ok(keys.includes('Ctrl-Alt-G'));
});
