// Issue #218: the leftovers of #208, #212 and #214. Graph and DAG hints in plain words first
// (through the one nextStep formatter), the Hide tool calls toggle, the Getting started help
// view, the Vim-keys tooltip and the platform key label on the Issues retry banner.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { graphEdgeHints, graphFooterHints, graphPickerHints } from '../dist/graph-model.js';
import { footerText, initialDag } from '../dist/dag-model.js';
import { hintLine, nextStep } from '../dist/key-hints.js';
import { VIM_KEYS_TIP } from '../dist/keyengine.js';
import { HELP_VIEWS, VIEW_TITLES, filterRows, gettingStartedRows } from '../dist/keyhelp-model.js';
import { moreActionsHint, emptyText } from '../dist/empty-states.js';
import { retryBannerText } from '../dist/issues-model.js';
import { actionSpec } from '../dist/registry.js';

const file = (p) => readFileSync(fileURLToPath(new URL(p, import.meta.url)), 'utf8');

test('nextStep: a plain action keeps its keys secondary and drops them when only Vim keys do it', () => {
  assert.equal(nextStep({ do: 'Delete the node', key: 'd' }), 'Delete the node (d)');
  assert.equal(nextStep({ do: 'Delete the node', key: 'd' }, false), '');
  assert.equal(nextStep({ do: 'Open the node detail', key: 'Enter' }, false), 'Open the node detail (Enter)');
  assert.equal(nextStep({ click: 'Run', key: 'r', to: 'run the workflow' }), 'Click Run (or press r) to run the workflow');
  assert.equal(nextStep({ click: 'Run', key: 'r', to: 'run the workflow' }, false), 'Click Run to run the workflow');
  assert.equal(hintLine([{ do: 'Undo', key: 'u' }, { do: 'Go back', key: 'Esc' }], false), 'Go back (Esc)');
});

test('graph footer: plain words first, keys in brackets; Vim keys off keeps only what works', () => {
  const full = graphFooterHints(false);
  assert.ok(full.startsWith('Click a node in the graph (or press h / j / k / l) to select it · '));
  assert.match(full, /Delete the node \(d\)/);
  assert.match(full, /Click Run \(or press r\) to save, check and run the workflow/);
  assert.match(full, /Click Close \(or press q\)$/);
  const off = graphFooterHints(false, false);
  assert.match(off, /^Click a node in the graph to select it · Open the node detail \(Enter\)/);
  assert.doesNotMatch(off, /\(d\)|press|\(Tab\) · Click Close \(or/);
  assert.match(off, /Switch to the dashboard \(Tab\) · Click Close$/);
  assert.equal(graphFooterHints(true), 'Click Starter (or press t) to start from a template · Click Saved (or press o) to open a saved workflow · Pan the view (H / J / K / L) · Follow the running node with the camera (c) · Click Close (or press q)');
  assert.equal(graphFooterHints(true, false), 'Click Starter to start from a template · Click Saved to open a saved workflow · Click Close');
});

test('graph side hints: edge detail (authored or not) and picker', () => {
  assert.equal(graphEdgeHints(true), 'Rename the node (i) · Edit the instructions (I) · Move between edges (j / k) · Delete the edge (x) · Follow an edge ([ / ]) · Go back (Esc)');
  assert.equal(graphEdgeHints(false), 'Edit the instructions (I) · Change the strategy (s / S) · Move between edges (j / k) · Follow an edge ([ / ]) · Go back (Esc)');
  assert.equal(graphEdgeHints(true, false), 'Go back (Esc)');
  assert.equal(graphPickerHints(), 'Move through the list (j / k) · Jump to the first or last (g / G) · Page through it (PageDown / PageUp) · Open the selected one (Enter) · Go back (Esc)');
  assert.equal(graphPickerHints(false), 'Page through it (PageDown / PageUp) · Open the selected one (Enter) · Go back (Esc)');
});

test('DAG footer: plain words for the browser and the inspector', () => {
  const s = initialDag();
  const f = footerText(s);
  assert.match(f, /^Click a panel tab \(or press Tab\) to switch panels · Move through the list \(j \/ k\) · Load or inspect the selected row \(Enter\)/);
  assert.match(f, /Click Reload \(or press r\) · /);
  assert.match(f, /Click Close \(or press q\)$/);
  assert.equal(footerText(s, false), 'Click a panel tab (or press Tab) to switch panels · Load or inspect the selected row (Enter) · Click Reload · Click Close');
  const ins = footerText({ ...s, inspector: { ...s.inspector, open: true } });
  assert.match(ins, /^Step between artifact rows \(\]a \/ \[a\) · Show the metadata \(m\)/);
  assert.match(ins, /Close the inspector \(Esc\)$/);
  assert.equal(footerText({ ...s, inspector: { ...s.inspector, open: true } }, false), 'Close the inspector (Esc)');
});

test('the one key-label path: nextStep and the empty states spell Ctrl as Cmd on macOS', () => {
  assert.equal(nextStep({ click: 'Actions…', key: 'Ctrl+K' }, true, 'mac'), 'Click Actions… (or press Cmd+K)');
  assert.equal(moreActionsHint('mac'), 'Click Actions… (or press Cmd+K) for every action');
  assert.equal(moreActionsHint('other'), 'Click Actions… (or press Ctrl+K) for every action');
  assert.equal(emptyText('prompts', true, 'mac'), 'No prompts yet. Click New (or press Cmd+N) to create one.');
  assert.equal(emptyText('prompts'), 'No prompts yet. Click New (or press Ctrl+N) to create one.');
  assert.match(file('../src/key-hints.ts'), /import \{ proseLabel \} from '\.\/keylabel\.js'/);
});

test('the Issues retry banner uses the platform key label', () => {
  assert.equal(retryBannerText('Issue created', 'mac'), 'Saving “Issue created” may have failed. Try again (Cmd-Enter)');
  assert.equal(retryBannerText('Issue created', 'other'), 'Saving “Issue created” may have failed. Try again (Ctrl-Enter)');
});

test('the conversation toggle says tool calls and keeps its action id', () => {
  assert.match(file('../static/index.html'), /data-action="toggle-tools">Hide tool calls</);
  assert.match(file('../src/conversation.ts'), /hide \? 'Show tool calls' : 'Hide tool calls'/);
  assert.equal(actionSpec('toggle-tools').title, 'Show tool calls');
  assert.doesNotMatch(file('../src/conversation.ts'), /Hide tools/);
});

test('Getting started is a help view with what a project, session and manager are and three first steps', () => {
  assert.ok(HELP_VIEWS.includes('start'));
  assert.equal(VIEW_TITLES.start, 'Getting started');
  const rows = gettingStartedRows('other');
  assert.deepEqual(rows.filter((r) => r.section === 'What these are').map((r) => r.title), ['Project', 'Session', 'Manager']);
  assert.deepEqual(rows.filter((r) => r.section === 'First three things to do').map((r) => r.title), ['1. Create a project', '2. Start a session', '3. Find any action']);
  assert.deepEqual(rows.find((r) => r.title === '2. Start a session').keys, ['Ctrl-N']);
  assert.deepEqual(gettingStartedRows('mac').find((r) => r.title === '2. Start a session').keys, ['Cmd-N']);
  assert.equal(filterRows(rows, 'manager').length >= 1, true);
  assert.match(file('../src/keyhelp.ts'), /start: gettingStartedRows\(currentPlatform\(\)\)/);
});

test('the Vim keys switch explains what turning it on changes, in the top bar, Settings and the palette', () => {
  assert.match(VIM_KEYS_TIP, /^Vim keys on: single letters such as j, k and a act on the app/);
  assert.match(VIM_KEYS_TIP, /Off: use the buttons and menus/);
  assert.ok(file('../static/index.html').includes(`title="${VIM_KEYS_TIP}"><input id="vim-keys"`));
  assert.match(file('../src/settings.ts'), /: VIM_KEYS_TIP\)/);
  assert.equal(actionSpec('toggle-vim-keys').detail, VIM_KEYS_TIP);
});
