// Dialog and pane key hints, tooltips and toasts follow the Vim keys switch (local #129, #132).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { formatKeyHint, formatKeyTip, offText, stripKeyLabel } from '../dist/key-hints.js';

test('button labels drop only the single-key suffix', () => {
  assert.equal(stripKeyLabel('Refresh (r)'), 'Refresh');
  assert.equal(stripKeyLabel('Delete (dd)'), 'Delete');
  assert.equal(stripKeyLabel('Enable/disable (Space)'), 'Enable/disable');
  assert.equal(stripKeyLabel('Answers: on (n)'), 'Answers: on');
  assert.equal(stripKeyLabel('Close'), 'Close');
  assert.equal(stripKeyLabel('Settings (all)'), 'Settings (all)');
  assert.equal(offText('button', 'Purge (D)'), 'Purge');
});

test('the Scheduled jobs hint keeps Enter and Esc and drops the letters', () => {
  const on = 'j/k move, n new, Enter/e edit, Space enable or disable, t run now, dd delete, r refresh, H history, m more, q/Esc close.';
  assert.equal(formatKeyHint(on), 'Enter edit, Esc close.');
});

test('budgets, confirm and admin hints', () => {
  assert.equal(formatKeyHint('j/k move · Enter or Space edit · a add · d delete · R refresh · Esc or q close'), 'Enter edit · Esc close');
  assert.equal(formatKeyHint('y sends · n or Esc cancels (Enter does not confirm)'), 'Esc cancels (Enter does not confirm)');
  assert.equal(formatKeyHint('y or Enter sends · n or Esc cancels'), 'Enter sends · Esc cancels');
  assert.equal(formatKeyHint('j/k pick live work · Enter or c cancels it (asks) · X stops all model work (asks) · R refresh · Esc or q close'),
    'Enter cancels it (asks) · Esc close');
  assert.equal(formatKeyHint('Enter sends, s (empty input) shows the sources, Up/Down scroll, Esc clears then closes, q closes an empty input.'),
    'Enter sends, Up/Down scroll, Esc clears then closes.');
  assert.equal(formatKeyHint('No scheduled jobs. Press n to create one.'), 'No scheduled jobs.');
  assert.equal(formatKeyHint('Type #RRGGBB (applies at 7 characters) · Enter apply · Delete clear · Esc or q close · Space N new session · Space ; palette'),
    'Type #RRGGBB (applies at 7 characters) · Enter apply · Delete clear · Esc close');
});

test('pane hint lines (managers) keep standard keys, Tab and Esc', () => {
  assert.equal(formatKeyHint('j/k · Enter open session · r refresh · Esc close'), 'Enter open session · Esc close');
  assert.equal(formatKeyHint('j/k g/G move · h/l Space H/L fold · Enter open · o project · Esc close'), 'Enter open · Esc close');
  assert.equal(formatKeyHint('1-5 · Tab/Shift-Tab sections · j/k fields · Enter/Space edit · s save · r reload · Esc close'),
    'F6/Shift-F6 sections · Enter edit · Esc close');
  assert.equal(formatKeyHint('Tab / Shift-Tab fields · Up/Down cycle scope kind and model tier · Enter saves · Esc closes'),
    'Tab / Shift-Tab fields · Up/Down cycle scope kind and model tier · Enter saves · Esc closes');
});

test('a hint with no Vim-only key is unchanged', () => {
  for (const t of ['Loading…', 'Type to filter · Enter picks · Esc closes', 'Comma-separated']) assert.equal(formatKeyHint(t), t);
});

test('the DOM pass is wired to the switch and marks <html>', () => {
  const vim = readFileSync(fileURLToPath(new URL('../src/vimkeys.ts', import.meta.url)), 'utf8');
  assert.match(vim, /installKeyHints\(document\.body, isVimKeysOn\)/);
  assert.match(vim, /writeVimKeys\(storage\(\), on\);\s*refreshKeyHints\(\)/);
  const mod = readFileSync(fileURLToPath(new URL('../src/key-hints.ts', import.meta.url)), 'utf8');
  assert.match(mod, /dataset\.vimKeys = isOn\(\) \? 'on' : 'off'/);
});

test('tooltips drop a parenthesised Vim-only key and keep Ctrl chords, Enter and Esc', () => {
  assert.equal(formatKeyTip('Pinned (P unpins)'), 'Pinned');
  assert.equal(formatKeyTip('Manual testing needed (<Space>t clears)'), 'Manual testing needed');
  assert.equal(formatKeyTip('Automatic retry attempt / budget (<Space>k cancels a pending retry)'), 'Automatic retry attempt / budget');
  assert.equal(formatKeyTip('Operator to-do (<Space>dt shows or hides the panel)'), 'Operator to-do');
  assert.equal(formatKeyTip('Clear the highlights (<Space>da)'), 'Clear the highlights');
  assert.equal(formatKeyTip('Refresh (r, :refresh)\nReload the list'), 'Refresh\nReload the list');
  assert.equal(formatKeyTip('Command palette… (Ctrl-K, <Space>;, :commands)\nSearch every action'), 'Command palette… (Ctrl-K)\nSearch every action');
  assert.equal(formatKeyTip('New session (Ctrl-N, <Space>N, :blank)'), 'New session (Ctrl-N)');
  assert.equal(formatKeyTip('Open this session (Enter in the inspector)'), 'Open this session (Enter in the inspector)');
  assert.equal(formatKeyTip('Open (Enter, l, L)'), 'Open (Enter)');
});

test('other parentheses and plain tooltips are unchanged', () => {
  for (const t of ['Settings (all)', '3 children, 5 total (some running)', 'Sessions (n of m)', 'Open a starter topology', 'Archive (a project)', '', 'Cmd-K opens the palette']) {
    assert.equal(formatKeyTip(t), t);
  }
});

test('toasts drop key clauses, "Press x" sentences and a trailing key', () => {
  assert.equal(formatKeyTip('Open a session first (j/k, then Enter)'), 'Open a session first (Enter)');
  assert.equal(formatKeyTip('Open a session first: click one in the list (j/k, then Enter)'), 'Open a session first: click one in the list (Enter)');
  assert.equal(formatKeyTip('Land b @ c: unmerged? Choose Land session branch again (<Space>gl) within 30s to queue it.'), 'Land b @ c: unmerged? Choose Land session branch again within 30s to queue it.');
  assert.equal(formatKeyTip('Here n launches a project manager; appoint portfolio and area seats from the manager tree (a)'),
    'Appoint portfolio and area seats from the manager tree');
  assert.equal(formatKeyTip('This node has no seat session to open; appoint one from the manager tree (a)'),
    'This node has no seat session to open; appoint one from the manager tree');
  assert.equal(formatKeyTip('No global manager session to open; n launches one'), 'No global manager session to open');
  assert.equal(formatKeyTip('Session abc12345 is Archived and read-only; n launches a manager'), 'Session abc12345 is Archived and read-only');
  assert.equal(formatKeyTip('Session abc12345 unavailable: connection refused; press r'), 'Session abc12345 unavailable: connection refused');
  assert.equal(formatKeyTip('Nothing to open: n launches a manager, or focus a session'), 'Nothing to open: focus a session');
  assert.equal(formatKeyTip('INTERRUPT NOW cancels the active turn. Press X again within 5 seconds to confirm.'), 'INTERRUPT NOW cancels the active turn.');
  assert.equal(formatKeyTip('No facts yet: a adds one.'), 'No facts yet.');
  assert.equal(formatKeyTip('No explicit budget policies: the daemon’s built-in per-scope defaults apply (a adds one)'),
    'No explicit budget policies: the daemon’s built-in per-scope defaults apply');
  assert.equal(formatKeyTip('The daemon config is not loaded (R retries)'), 'The daemon config is not loaded');
});

test('toasts with no Vim-only key are unchanged', () => {
  for (const t of ['Sent', 'Send failed: Error: connection refused', 'Failed to load: a project is missing', 'The preset is granted as is',
    'The desktop has no tabs or splits: the session opens in the list', 'Inbound policy saved']) assert.equal(formatKeyTip(t), t);
});

test('offText routes the tip kind to formatKeyTip', () => {
  assert.equal(offText('tip', 'Pinned (P unpins)'), 'Pinned');
});

test('every registry tooltip key is classified: Vim-only keys go, Ctrl and Enter stay', async () => {
  const { KEY_BINDINGS, COMMAND_BINDINGS } = await import('../dist/keybindings.js');
  for (const k of Object.keys(KEY_BINDINGS)) {
    const out = formatKeyTip(`Act (${k})`);
    if (/^(Ctrl|Cmd|Alt)-|^Enter$/.test(k)) assert.equal(out, `Act (${k})`, k);
    else assert.equal(out, 'Act', `key ${k}`);
  }
  for (const c of Object.keys(COMMAND_BINDINGS)) assert.equal(formatKeyTip(`Act (:${c})`), 'Act', `command :${c}`);
});

test('the DOM pass also covers title attributes and the toast element', () => {
  const mod = readFileSync(fileURLToPath(new URL('../src/key-hints.ts', import.meta.url)), 'utf8');
  assert.match(mod, /attributeFilter: \['title'\]/);
  assert.match(mod, /TIP_SELECTOR = '#toast,/);
  assert.match(mod, /isOn\(\) \? orig : formatKeyTip\(orig\)/);
});

// ---------- local #133: the corners the pass did not reach ----------

test('the to-do footer drops its leader keys (and has no keyless form for the undo line)', () => {
  assert.equal(formatKeyHint('<Space>dj/dk move · dv open · dd done · dx dismiss · dy copy · do link'), '');
  assert.equal(formatKeyHint('<Space>du undoes “Fix a, b; c. d” (4 s)'), '');
  const todo = readFileSync(fileURLToPath(new URL('../src/todo.ts', import.meta.url)), 'utf8');
  assert.match(todo, /foot\.dataset\.keyHint = ''/);
});

test('Settings value cells keep Enter and drop the letter keys', () => {
  assert.equal(formatKeyHint('▾ Enter lists · a adds'), '▾ Enter lists');
  assert.equal(formatKeyHint('▾ Enter opens · s r c d i'), '▾ Enter opens');
  const settings = readFileSync(fileURLToPath(new URL('../src/settings.ts', import.meta.url)), 'utf8');
  assert.match(settings, /'▾ Enter lists · a adds'; box\.dataset\.keyHint = ''/);
  assert.match(settings, /'▾ Enter opens · s r c d i'; box\.dataset\.keyHint = ''/);
});

test('the notification log keeps toast text raw and shows it through the tip rule', () => {
  const life = readFileSync(fileURLToPath(new URL('../src/actions/lifecycle.ts', import.meta.url)), 'utf8');
  assert.match(life, /span\.dataset\.keyTip = ''/);
  assert.match(life, /li\.append\(noticeMsg\(n\.message\)/);
  assert.match(life, /message: msg \}/); // recorded raw: the original is restorable
  assert.equal(offText('tip', 'Nothing to open: n launches a manager, or focus a session and run :manager global appoint'),
    'Nothing to open: focus a session and run “Appoint global manager” (command palette)');
});

test('toast prose that tells the operator to run a : command points at the palette entry', () => {
  assert.equal(formatKeyTip('No manager is appointed: :manager appoint first'), 'No manager is appointed: “Appoint harness manager” (command palette) first');
  assert.equal(formatKeyTip('Use :manager appoint on it.'), 'Use “Appoint harness manager” (command palette) on it.');
  assert.equal(formatKeyTip('Use :manual [pager|pdf]'), 'Use “Open manual” (command palette)');
  assert.equal(formatKeyTip('name one: :topology-resolve <execution_id> <action>'), 'name one: “Resolve preserved topology work” (command palette)');
  assert.equal(formatKeyTip('Hooks not saved: the file changed, reopen :hooks to reload'), 'Hooks not saved: the file changed, reopen “Open hook settings” (command palette) to reload');
  assert.equal(formatKeyTip('Enter: not a command, 12:30 is a time'), 'Enter: not a command, 12:30 is a time');
});

test('every registered command and alias in prose has a palette title', async () => {
  const { COMMANDS } = await import('../dist/keymap.generated.js');
  for (const c of COMMANDS) for (const n of [c.name, ...c.aliases]) {
    assert.equal(formatKeyTip(`run :${n}`), `run “${c.title}” (command palette)`, n);
  }
});

test('admin prose with a key list inside a sentence loses the letter keys', () => {
  assert.equal(formatKeyHint('Scratch reclaim is enabled: an adopted directory that is old enough and unheld is deleted automatically later. a / Enter confirms the selected one, A every adoptable one (at most 32); nothing is sent until y. Esc / n / q cancels a confirmation or closes; r refreshes.'),
    'Scratch reclaim is enabled: an adopted directory that is old enough and unheld is deleted automatically later. Enter confirms the selected one; nothing is sent until you confirm. Esc cancels a confirmation or closes.');
  assert.equal(formatKeyHint('Tab cycles peers, links and cached sessions; j/k select; a / e add or edit; d enables or disables; R repairs a quarantined peer; p probes a link; I edits this daemon\'s inbound policy; n / b page cached sessions; r refresh; q/Esc close. Reads only the hub daemon.'),
    'F6 cycles peers, links and cached sessions; Esc close. Reads only the hub daemon.');
  assert.equal(formatKeyHint('j/k (J/K page) select; Enter audits (read-only); A begins authorization after a fresh applyable audit; r refreshes the receipt; Esc / q close; Space N new session, Space ; palette.'),
    'Enter audits (read-only); Esc close.');
  assert.equal(formatKeyHint('j/k move, s or Enter set, r rotate, c check (on a login or contract row: probe now), d twice clears, i imports legacy env keys, R refreshes, q/Esc close. Key values are write-only.'),
    'Enter set, Esc close. Key values are write-only.');
});

test('plain sentences that merely start with a letter-like word are left alone', () => {
  for (const t of ['Saved. A fresh audit is required; a project is chosen.', 'Nothing to show. I think it is fine, a bit odd; Enter opens.'])
    assert.equal(formatKeyHint(t), t);
});

test('Settings info-card facts and generated row summaries drop letter keys with the switch off (local #139)', () => {
  const cases = [
    ['Flips the switch (staged; w applies)', 'Flips the switch (staged)'],
    ['Next choice (staged; + / - step, w applies)', 'Next choice (staged)'],
    ['Type a value (staged; w applies)', 'Type a value (staged)'],
    ['Opens the budget policies (a adds, Enter edits, d deletes)', 'Opens the budget policies (Enter edits)'],
    ['Opens Provider keys (s sets, r rotates, c checks, d clears, i imports; values are never shown)', 'Opens Provider keys (values are never shown)'],
    ['Daemon model budget policies; a adds, Enter edits, d deletes (an empty list shows the add hint).', 'Daemon model budget policies; Enter edits.'], // the add hint it mentions goes with the key
    ['Twenty-three masked vault slots; s sets, r rotates, c checks, d clears after confirmation, and i imports from environment.', 'Twenty-three masked vault slots.'],
    ['Claude Code hooks from settings.json; Enter edits, a adds, d deletes.', 'Claude Code hooks from settings.json; Enter edits.'],
    ['Daemon-configured MCP server definitions; a adds, Enter edits, t enables or disables.', 'Daemon-configured MCP server definitions; Enter edits.'],
    ['Claude user skills; Enter previews, e enables or disables.', 'Claude user skills; Enter previews.'],
    ['Vim is the modal editing. Applies when you apply the page (w), with no restart; `unset` asks again.', 'Vim is the modal editing. Applies when you apply the page, with no restart; `unset` asks again.'],
    ['Shows sandbox storage usage; R refreshes it.', 'Shows sandbox storage usage.'],
  ];
  for (const [on, off] of cases) assert.equal(formatKeyTip(on), off, on);
  // Prose with no key list, and an Enter-only list, stay as written.
  for (const same of ['Opens the theme picker', 'Opens the hex editor (#RRGGBB; Delete clears)', 'Next, a creative option is offered', 'Enter edits, Esc cancels']) assert.equal(formatKeyTip(same), same);
});

test('Settings wiring: the info card, group and section summaries carry the tip rule; row-key facts the hint rule', () => {
  const settings = readFileSync(fileURLToPath(new URL('../src/settings.ts', import.meta.url)), 'utf8');
  assert.match(settings, /card\.dataset\.keyTip = ''/);
  assert.match(settings, /infoShell\.dataset\.keyTip = ''/);
  assert.match(settings, /sum\.dataset\.keyTip = ''/);
  assert.match(settings, /sectionSummary\.dataset\.keyTip = ''/);
  assert.match(settings, /fact\(k, what, true\)/);
  assert.equal(formatKeyHint('a'), ''); // a row-key label empties; the CSS hides its pair
  assert.equal(formatKeyHint('Enter'), 'Enter');
  const css = readFileSync(fileURLToPath(new URL('../src/settings-css.ts', import.meta.url)), 'utf8');
  assert.match(css, /dt:empty \+ dd \{ display: none; \}/);
});

test('isVimOnlyHint: chords, F-keys and standard keys work with the switch off; letters, leaders and : commands do not', async () => {
  const { isVimOnlyHint, formatKeyTip } = await import('../dist/key-hints.js');
  for (const k of ['Ctrl-K', 'Cmd-K', 'Ctrl-Alt-G', 'Shift-Down', 'F2', 'Enter', 'Esc', 'Tab', 'PageDown', 'Up']) assert.equal(isVimOnlyHint(k), false, k);
  for (const k of ['j', 'gg', 'DD', 'ZQ', '<Space>a', ':archive', ':', '/', '?', ']a', 'Backspace', 'yy']) assert.equal(isVimOnlyHint(k), true, k);
  // A "then" step and a `:q!` form are Vim keys in a tip too (local #227).
  assert.equal(formatKeyTip('Every cap as used / limit (gm, then b)'), 'Every cap as used / limit');
  assert.equal(formatKeyTip('Closes the viewer (q, <Space>q or :q; :q! discards unsaved changes)'), 'Closes the viewer');
  assert.equal(formatKeyTip('Open a session first (j/k, then Enter)'), 'Open a session first (Enter)');
});
