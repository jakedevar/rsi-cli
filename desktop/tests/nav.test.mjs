// Gap sweep G2 (hub #3000): pane, zone and window navigation, the core keys
// it claims, the default launch model and the reserved TUI
// sequences.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, keyHints, unavailable } from '../dist/registry.js';
import { CORE_KEY_BINDINGS, KEY_BINDINGS, desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import { NORMAL_KEYS, RESERVED_KEYS } from '../dist/keymap.generated.js';
import { NAV_ACTIONS, NAV_COMMANDS, NAV_KEYS, NAV_RULES } from '../dist/actions/nav.js';
import {
  backStep, closeTarget, isJobSession,
  isWebUrl, looksLikePath, modelArg, modelIds, nextProvider, parseLaunchDefault, resolvePath,
  routeNavKey, scrollBy, stepProject, stepZone, terminalCommand, zoneLabel,
} from '../dist/nav-model.js';
import { buildTree, flattenTree } from '../dist/tree.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

const snap = (over = {}, nav = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null,
  nav: { zone: 'main', back: null, projects: 2, ...nav },
  ...over,
});

const feed = (keys) => {
  const e = new KeyEngine(desktopKeymap());
  let out;
  for (const k of keys) out = e.feed(k);
  return out;
};

// ---------- registry and keys ----------

test('every TUI Normal-mode key now runs a desktop action', () => {
  const unbound = NORMAL_KEYS.filter((r) => !KEY_BINDINGS[r.keys]).map((r) => r.keys);
  // The last one, <Space>gP, is the prompt creator over the hub #3040 prompt-file RPCs (local #88).
  assert.deepEqual(unbound, []);
});

test('the navigation actions are registered with their TUI keys and commands', () => {
  const ids = NAV_ACTIONS.map((a) => a.id);
  assert.deepEqual(Object.keys(NAV_RULES).sort(), [...ids].sort());
  for (const id of ids) assert.ok(ACTIONS.some((a) => a.id === id), id);
  assert.deepEqual(keyHints('view-jobs'), ['gj']);
  assert.deepEqual(keyHints('ascend-or-back'), ['Backspace']);
  assert.deepEqual(keyHints('close-pane'), ['<Space>q', ':close']);
  assert.deepEqual(keyHints('next-tab'), ['>', ':tabnext']);
  assert.deepEqual(keyHints('prev-tab'), ['<', ':tabprev']);
  assert.deepEqual(keyHints('session-normal'), ['e']);
  assert.deepEqual(keyHints('model-dropdown'), ['Ctrl-M', ':model']);
  assert.deepEqual(keyHints('git-panel'), ['<Space>gg']);
  assert.deepEqual(keyHints('system-monitor'), ['<Space>bb']);
  assert.ok(keyHints('open').includes('<Space>T'), '<Space>T opens the session in place');
  assert.deepEqual(Object.keys(NAV_COMMANDS).sort(), ['close', 'model', 'tabnext', 'tabprev']);
  // gf1..gf9 have one implementation: gap sweep G1's open-recent-file (actions/input.ts).
  assert.deepEqual(feed(['g', 'f', '3']), { kind: 'run', action: 'open-recent-file', count: null, keys: 'gf3', arg: '3' });
  assert.equal(feed(['Backspace']).action, 'ascend-or-back');
  assert.equal(feed(['C-m']).action, 'model-dropdown');
  assert.equal(feed(['Space', 'b', 'b']).action, 'system-monitor');
  assert.equal(feed(['>']).action, 'next-tab');
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('model gpt-x').arg, 'gpt-x');
  assert.equal(NAV_KEYS['<Space>T'].action, 'open');
});

test('i / a / o / O reach the one focus-composer action with their key (G1 places the caret)', () => {
  assert.deepEqual(CORE_KEY_BINDINGS.i, { action: 'focus-composer', arg: 'i' });
  for (const k of ['a', 'o', 'O']) assert.deepEqual(feed([k]), { kind: 'run', action: 'focus-composer', count: null, keys: k, arg: k });
});

test('the reserved TUI sequences: prefixes wait, retired chords stay inert', () => {
  for (const r of RESERVED_KEYS) {
    const keys = r.keys === '<Space>' ? ['Space'] : r.keys.startsWith('<Space>') ? ['Space', ...r.keys.slice(7)] : [...r.keys];
    const out = feed(keys);
    if (r.kind === 'prefix') assert.equal(out.kind, 'pending', `${r.keys} is a prefix`);
    else assert.deepEqual(out, { kind: 'retired', keys: r.keys, why: r.why }, `${r.keys} is retired`);
  }
  assert.equal(feed(['Space', 'b']).kind, 'pending', '<Space>b is the system-tools prefix');
  assert.equal(feed(['Space', 'g']).kind, 'pending', '<Space>g is the manager and Git prefix');
});

test('the rules say why a navigation cannot run', () => {
  assert.equal(unavailable('ascend-or-back', snap()), 'At the top level');
  assert.equal(unavailable('ascend-or-back', snap({}, { back: 'to-list' })), null);
  assert.equal(unavailable('close-pane', snap({ open: null })), 'No pane to close');
  assert.equal(unavailable('close-pane', snap()), null);
  assert.equal(unavailable('next-tab', snap({}, { projects: 0 })), 'There are no projects yet');
  assert.equal(unavailable('session-normal', snap({ open: null, cursorOnRow: false })), 'No session under the cursor (j/k to move)');
  assert.equal(unavailable('view-jobs', snap()), null);
});

// ---------- model ----------

test('the Jobs zone holds the root sessions scheduled jobs launched', () => {
  assert.equal(isJobSession({ scheduled_job_id: 'j' }), true);
  assert.equal(isJobSession({ scheduled_job_id: 'j', parent_id: 'p' }), false, 'a woken session stays in its hierarchy');
  assert.equal(isJobSession({}), false);
  const s = (id, over) => ({ id, status: 'Completed', provider: 'Claude', query: id, working_dir: '/w', created_at: '2026-10-18T00:00:00Z', updated_at: '2026-10-18T00:00:00Z', ...over });
  const all = [s('job', { scheduled_job_id: 'j' }), s('live', { status: 'Running' }), s('jobRun', { status: 'Running', scheduled_job_id: 'j' })];
  const ids = (zone, showFinished = false) => flattenTree(buildTree(all, { projectId: null, text: '', showFinished, mode: 'active', zone }), new Set()).map((r) => r.session.id).sort();
  assert.deepEqual(ids('main', true), ['live']);
  assert.deepEqual(ids('jobs'), ['job', 'jobRun'], 'Jobs lists finished runs too');
  assert.deepEqual(ids(undefined, true), ['job', 'jobRun', 'live']);
  assert.equal(stepZone('main', 1), 'jobs');
  assert.equal(stepZone('archived', 1), 'main');
  assert.equal(stepZone('main', -1), 'archived');
  assert.match(zoneLabel('jobs'), /Jobs/);
});

test('Backspace goes back to the list from the transcript, else up a container', () => {
  assert.equal(backStep('transcript', true, true), 'to-list');
  assert.equal(backStep('list', true, true), 'ascend');
  assert.equal(backStep('list', false, true), null);
  assert.equal(backStep('transcript', false, false), null);
});

test('close pane picks the topmost pane, the open session last', () => {
  assert.equal(closeTarget({ open: {}, terminal: { visible: true }, files: { viewer: true, explorer: true } }), 'toggle-terminal');
  assert.equal(closeTarget({ open: {}, files: { viewer: true, explorer: true } }), 'files-close-viewer');
  assert.equal(closeTarget({ open: {}, files: { viewer: false, explorer: true } }), 'files-explorer');
  assert.equal(closeTarget({ open: null, graphs: { pane: 'dag' } }), 'dag-close');
  assert.equal(closeTarget({ open: null, graphs: { pane: 'graph' } }), 'graph-close');
  assert.equal(closeTarget({ open: null, managers: { view: 'board' } }), 'mgr-close');
  assert.equal(closeTarget({ open: null, issues: { active: true } }), 'issues-close');
  assert.equal(closeTarget({ open: {} }), 'session');
  assert.equal(closeTarget({ open: null }), null);
});

test('the project tabs wrap through All projects', () => {
  assert.equal(stepProject(['a', 'b'], '', 1), 'a');
  assert.equal(stepProject(['a', 'b'], 'b', 1), '');
  assert.equal(stepProject(['a', 'b'], '', -1), 'b');
  assert.equal(stepProject(['a', 'b'], 'gone', 1), 'a');
});

test('transcript paths and links', () => {
  for (const p of ['src/a.ts', '/abs/b.rs', 'Cargo.toml', './x/y', 'docs/keybindings.md']) assert.ok(looksLikePath(p), p);
  for (const p of ['hello world', 'https://e.com/a.b', 'fn()', '', 'a', 'x<y']) assert.equal(looksLikePath(p), false, p);
  assert.equal(resolvePath('/w', 'src/a.ts'), '/w/src/a.ts');
  assert.equal(resolvePath('/w/', './a.ts'), '/w/a.ts');
  assert.equal(resolvePath('/w', '/abs/a.ts'), '/abs/a.ts');
  assert.equal(resolvePath(null, 'a.ts'), 'a.ts');
  assert.ok(isWebUrl('https://e.com/x'));
  assert.equal(isWebUrl('file:///etc/passwd'), false);
});

test('window and scroll keys follow the modalkit tables; G1 routes the Global keys', () => {
  const k = (key, mods = {}, armed = false) => routeNavKey({ key, ...mods }, armed);
  assert.deepEqual(k('d', { ctrlKey: true }), { kind: 'scroll', amount: 'half', dir: 1 });
  assert.deepEqual(k('b', { ctrlKey: true }), { kind: 'scroll', amount: 'page', dir: -1 });
  assert.deepEqual(k('w', { ctrlKey: true }), { kind: 'arm-window' });
  assert.deepEqual(k('j', {}, true), { kind: 'focus', to: 'composer' });
  assert.deepEqual(k('h', {}, true), { kind: 'focus', to: 'list' });
  // Arrows, Ctrl-Tab, Ctrl-h / l, Ctrl-arrows and Ctrl-Shift-arrows are G1's routeGlobal (one owner).
  for (const [key, mods] of [['ArrowLeft', {}], ['Tab', { ctrlKey: true }], ['h', { ctrlKey: true }], ['l', { ctrlKey: true }],
    ['ArrowRight', { ctrlKey: true }], ['ArrowRight', { ctrlKey: true, shiftKey: true }]]) {
    assert.deepEqual(k(key, mods), { kind: 'pass' }, `${key} passes to G1`);
  }
  assert.deepEqual(k('j'), { kind: 'pass' }, 'plain j stays the engine’s');
  assert.deepEqual(k('d', { metaKey: true }), { kind: 'pass' });
  assert.equal(scrollBy('half', 401), 200);
  assert.equal(scrollBy('page', 400), 400);
});

test('the default launch model', () => {
  assert.deepEqual(parseLaunchDefault('{"provider":"Codex","model":"gpt-x"}'), { provider: 'Codex', model: 'gpt-x' });
  assert.equal(parseLaunchDefault('nope'), null);
  assert.equal(parseLaunchDefault('{"model":"x"}'), null);
  assert.deepEqual(modelIds([['a', 'A'], ['b', 'b'], ['a', 'again']]), [{ id: 'a', label: 'A' }, { id: 'b', label: 'b' }]);
  assert.deepEqual(modelIds({ models: ['m'] }), [{ id: 'm', label: 'm' }]);
  assert.deepEqual(modelIds(null), []);
  assert.equal(nextProvider(['Claude', 'Codex'], 'Codex'), 'Claude');
  assert.equal(nextProvider(['Claude', 'Codex'], 'Gone'), 'Claude');
  assert.equal(modelArg(' gpt-x '), 'gpt-x');
  assert.equal(modelArg(''), null);
});

test('lazygit and btop check they are installed in a POSIX shell', () => {
  assert.equal(terminalCommand('btop', true), 'btop');
  assert.equal(terminalCommand('lazygit', false), 'command -v lazygit >/dev/null 2>&1 && lazygit || echo "lazygit is not installed"');
});

test('the run side uses the commands and exports it relies on', () => {
  const run = readFileSync(here('../src/actions/nav-run.ts'), 'utf8');
  const rust = readFileSync(here('../src-tauri/src/nav.rs'), 'utf8');
  const todo = readFileSync(here('../src/todo.ts'), 'utf8');
  assert.match(rust, /pub fn open_url\(url: String\)/);
  assert.match(run, /'open_url', \{ url \}/);
  assert.match(todo, /'open_url', \{ url: link\.url \}/);
  assert.match(readFileSync(here('../src/commands.ts'), 'utf8'), /claim\?\(id: ActionId, inv: Invocation\): ActionId \| null;/);
  assert.match(readFileSync(here('../src/files.ts'), 'utf8'), /export function openPathInViewer\(path: string\)/);
  assert.match(readFileSync(here('../src/actions/terminal.ts'), 'utf8'), /export async function runInTerminal\(command: string\)/);
});
