// Local #228: the Vim keys switch is in the top bar where the copy says it is, Settings shows
// two plainly different rows, and the first-run card, Welcome and Settings head follow the switch.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { VIM_FIRST_RUN_NOTE, VIM_KEYS_TIP } from '../dist/keyengine.js';
import { formatKeyTip } from '../dist/key-hints.js';
import { onboardingView } from '../dist/onboarding-model.js';
import { gettingStartedRows } from '../dist/keyhelp-model.js';
import { VIM_KEYS_ROW, sectionRows } from '../dist/settings-model.js';

const read = (rel) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const connected = { kind: 'connected', socket: 's' };
const welcome = (vimOn) => onboardingView(connected, 0, 'mac', 2, vimOn).lines.join('\n');

test('the switch is in the top bar with a visible label and the shared tooltip', () => {
  const html = read('../static/index.html');
  const bar = html.slice(html.indexOf('<header class="topbar">'), html.indexOf('</header>'));
  assert.match(bar, /<label class="check" title="[^"]*"><input id="vim-keys" type="checkbox"> Vim keys<\/label>/);
  assert.ok(bar.includes(`title="${VIM_KEYS_TIP}"`));
  // Nothing moves it away: no parking div, and Settings never reparents the element.
  const settings = read('../src/settings.ts');
  assert.doesNotMatch(settings, /settings-parking|vimToggle/);
  assert.doesNotMatch(settings, /getElementById\('vim-keys'\)/);
  assert.doesNotMatch(read('../static/styles.css'), /settings-parking/);
});

test('the first-run card note points at the top bar and Settings, where the switch is', () => {
  assert.match(VIM_FIRST_RUN_NOTE, /Vim keys switch in the top bar/);
  assert.match(VIM_FIRST_RUN_NOTE, /Settings › Workspace › Editing Mode/);
  assert.match(read('../src/vimkeys.ts'), /el\('p', 'first-run-note', VIM_FIRST_RUN_NOTE\)/);
});

test('Welcome: off offers to turn Vim keys on; on keeps the Vim line and says where to turn them off', () => {
  const off = welcome(false);
  assert.match(off, /Use the rsi TUI's Vim keys\? Turn on Vim keys \(the switch in the top bar, or Settings › Workspace\)/);
  assert.doesNotMatch(off, /Its Vim keys work here/);
  const on = welcome(true);
  assert.match(on, /Know the TUI\? Its Vim keys work here \(\? lists them\)\./);
  assert.match(on, /Turn off Vim keys with the switch in the top bar, or in Settings › Workspace/);
  // A new install has the switch off, so the default view is the off wording.
  assert.equal(onboardingView(connected, 0, 'mac', 2).lines.join('\n'), off);
  assert.notEqual(JSON.stringify(onboardingView(connected, 0, 'mac', 2, true)), JSON.stringify(onboardingView(connected, 0, 'mac', 2, false)));
});

test('Settings › Workspace › Editing Mode: "Vim keys in this window" first, the daemon row named for the TUI', () => {
  const rows = sectionRows('editing_mode', {});
  assert.deepEqual(rows.map((r) => r.label), ['Vim keys in this window', 'Text editing in the TUI (Standard or Vim)']);
  assert.deepEqual(rows[0].widget, { type: 'local', local: 'vim-keys' });
  assert.equal(rows[0].id, VIM_KEYS_ROW.id);
  assert.match(rows[0].summary, /top bar/);
  assert.match(rows[1].summary, /does not change this window/);
  assert.equal(rows[1].id, 'editing_mode');
  assert.deepEqual(rows[1].widget, { type: 'choice', options: ['unset', 'standard', 'vim'] });
});

test('the Settings pending line names no "w" with the switch off', () => {
  const settings = read('../src/settings.ts');
  const line = /pending change[^·]*· (Apply saves [^`]*\(or press w\))`/.exec(settings)?.[1];
  assert.equal(line, "Apply saves ${pending === 1 ? 'it' : 'them'} (or press w)");
  assert.match(settings, /keyTip\(el\('span', 'meta settings-head-status'\)\)/);
  const text = '2 pending changes · Apply saves them (or press w)';
  assert.equal(formatKeyTip(text), '2 pending changes · Apply saves them');
  assert.match(text, /press w/);
});

test('Help › Getting started says where the Vim keys switch is', () => {
  const row = gettingStartedRows('other').find((r) => r.section === 'Vim keys');
  assert.ok(row);
  assert.match(row.detail, /top bar/);
  assert.match(row.detail, /Settings › Workspace › Editing Mode/);
});
