import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, appMenuModel, menuItems, keyHints, paletteEntries, tooltipText, unavailable } from '../dist/registry.js';
import { TERMINAL_ACTIONS, TERMINAL_COMMAND_BINDINGS, TERMINAL_RULES, isTerminalAction } from '../dist/actions/terminal-spec.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine, routeKey } from '../dist/keyengine.js';
import {
  DEFAULT_SIZE, EMPTY_TERMS, MAX_COLS, MAX_ROWS, NO_SESSION_KEY, chunkInput, clampGrid, exited, gridFor, halfPage,
  isRunning, isToggleChord, keyForPty, modeLabel, overlayRect, overlayTitle, remove, resizeStep, routeTerminalKey,
  runningPtys, spawnFailed, spawned, terminalKey, toggle,
} from '../dist/terminal-model.js';
import { VENDORED, stripSourceMap } from '../scripts/vendor-terminal.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const S1 = '11111111-1111-4111-8111-111111111111';
const S2 = '22222222-2222-4222-8222-222222222222';
const P1 = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';
const P2 = 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb';

const snap = (over = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false, event: null, ...over,
});
const key = (k, over = {}) => ({ key: k, ...over });

// ---------- state ----------

test('the first toggle shows the open session’s terminal and asks for a shell', () => {
  const step = toggle(EMPTY_TERMS, S1, '/work/a');
  assert.equal(step.kind, 'show');
  assert.equal(step.key, S1);
  assert.equal(step.spawn, true);
  assert.equal(step.state.visible, S1);
  assert.equal(step.state.mode, 'insert');
  assert.deepEqual(step.state.entries[S1], { key: S1, ptyId: null, cwd: '/work/a', exited: null });
});

test('hiding keeps the shell; showing again reuses it without a new spawn', () => {
  let s = toggle(EMPTY_TERMS, S1, '/work/a').state;
  s = spawned(s, S1, P1, '/work/a');
  assert.ok(isRunning(s.entries[S1]));
  const hide = toggle(s, S1, '/work/a');
  assert.equal(hide.kind, 'hide');
  assert.equal(hide.state.visible, null);
  assert.equal(hide.state.entries[S1].ptyId, P1, 'the shell keeps running when hidden');
  const again = toggle({ ...hide.state, mode: 'normal' }, S1, '/work/a');
  assert.equal(again.kind, 'show');
  assert.equal(again.spawn, false);
  assert.equal(again.state.mode, 'insert', 'opening lands in insert mode');
});

test('terminals are keyed by session; no open session uses the home terminal', () => {
  let s = spawned(toggle(EMPTY_TERMS, S1, '/a').state, S1, P1, '/a');
  s = toggle(s, S1, '/a').state; // hide
  const other = toggle(s, S2, '/b');
  assert.equal(other.key, S2);
  assert.equal(other.spawn, true);
  assert.equal(other.state.entries[S1].ptyId, P1, 'the first session keeps its shell');
  assert.equal(terminalKey(null), NO_SESSION_KEY);
  assert.equal(terminalKey(undefined), NO_SESSION_KEY);
  assert.equal(toggle(EMPTY_TERMS, null, '').key, NO_SESSION_KEY);
  s = spawned(other.state, S2, P2, '/b');
  assert.deepEqual(runningPtys(s).sort(), [P1, P2]);
  assert.equal(keyForPty(s, P2), S2);
  assert.equal(keyForPty(s, 'unknown'), null);
});

test('a toggle while the shell is still starting does not start a second one', () => {
  const s = toggle(toggle(EMPTY_TERMS, S1, '/a').state, S1, '/a').state; // shown then hidden, spawn pending
  const again = toggle(s, S1, '/a');
  assert.equal(again.spawn, false);
});

test('an exited shell is reported once and the next open starts a fresh one', () => {
  let s = spawned(toggle(EMPTY_TERMS, S1, '/a').state, S1, P1, '/a');
  const r = exited(s, P1, 0);
  assert.equal(r.key, S1);
  s = r.state;
  assert.equal(isRunning(s.entries[S1]), false);
  assert.deepEqual(s.entries[S1].exited, { code: 0 });
  assert.match(overlayTitle(s.entries[S1]), /exited \(code 0\)/);
  assert.deepEqual(runningPtys(s), []);
  assert.equal(exited(s, P1, 0).key, null, 'a second exit for the same id is ignored');
  s = toggle(s, S1, '/a').state; // hide
  const reopen = toggle(s, S1, '/a');
  assert.equal(reopen.spawn, true);
  assert.equal(reopen.state.entries[S1].exited, null);
});

test('a failed spawn forgets the entry and hides the overlay; remove ends a terminal', () => {
  const shown = toggle(EMPTY_TERMS, S1, '/a').state;
  const failed = spawnFailed(shown, S1);
  assert.equal(failed.visible, null);
  assert.equal(failed.entries[S1], undefined);
  const s = spawned(shown, S1, P1, '/a');
  const gone = remove({ ...s, mode: 'normal' }, S1);
  assert.equal(gone.entries[S1], undefined);
  assert.equal(gone.visible, null);
  assert.equal(gone.mode, 'insert');
  assert.equal(spawned(EMPTY_TERMS, S1, P1, '/a'), EMPTY_TERMS, 'a reply for a forgotten key changes nothing');
});

test('titles and mode labels', () => {
  assert.equal(overlayTitle(undefined), 'Terminal');
  assert.equal(overlayTitle({ key: S1, ptyId: null, cwd: '/a', exited: null }), 'Terminal — /a — starting…');
  assert.equal(overlayTitle({ key: S1, ptyId: P1, cwd: '/a', exited: null }), 'Terminal — /a');
  assert.match(overlayTitle({ key: S1, ptyId: null, cwd: '/a', exited: { code: null } }), /— exited; /);
  assert.equal(modeLabel('insert'), 'INSERT');
  assert.equal(modeLabel('normal'), 'NORMAL · i to type', 'normal mode names the way back');
  assert.equal(modeLabel('insert', false), '', 'no Vim keys, no mode label');
  assert.equal(modeLabel('normal', false), '');
});

// ---------- keys ----------

test('Ctrl-\\ toggles everywhere; on macOS Cmd-\\ too', () => {
  assert.ok(isToggleChord(key('\\', { code: 'Backslash', ctrlKey: true }), 'other'));
  assert.ok(isToggleChord(key('\x1c', { ctrlKey: true }), 'other'));
  assert.ok(isToggleChord(key('\\', { code: 'Backslash', ctrlKey: true }), 'mac'));
  assert.ok(isToggleChord(key('\\', { code: 'Backslash', metaKey: true }), 'mac'));
  assert.equal(isToggleChord(key('\\', { code: 'Backslash', metaKey: true }), 'other'), false);
  assert.equal(isToggleChord(key('\\', { code: 'Backslash' }), 'other'), false);
  assert.equal(isToggleChord(key('\\', { code: 'Backslash', ctrlKey: true, altKey: true }), 'other'), false);
  assert.equal(isToggleChord(key('\\', { code: 'Backslash', ctrlKey: true, isComposing: true }), 'other'), false);
});

test('insert mode sends every key to the shell except Esc and Ctrl-\\', () => {
  const r = (k, over) => routeTerminalKey('insert', key(k, over), 'other', 24);
  assert.deepEqual(r('a'), { kind: 'pty' });
  assert.deepEqual(r('j'), { kind: 'pty' }, 'vim keys are off in the terminal');
  assert.deepEqual(r(':'), { kind: 'pty' });
  assert.deepEqual(r('c', { code: 'KeyC', ctrlKey: true }), { kind: 'pty' }, 'Ctrl-C is the shell’s interrupt');
  assert.deepEqual(r('k', { code: 'KeyK', ctrlKey: true }), { kind: 'pty' }, 'Ctrl-K goes to the shell, not the palette');
  assert.deepEqual(r('Enter'), { kind: 'pty' });
  assert.deepEqual(r('Escape'), { kind: 'mode', mode: 'normal' });
  assert.deepEqual(r('Escape', { shiftKey: true }), { kind: 'pty' });
  assert.deepEqual(r('\\', { code: 'Backslash', ctrlKey: true }), { kind: 'toggle' });
  assert.deepEqual(routeTerminalKey('insert', key('c', { metaKey: true }), 'mac', 24), { kind: 'native' }, 'Cmd-C copies');
});

test('normal mode scrolls the scrollback the TUI way', () => {
  const r = (k, over, rows = 24) => routeTerminalKey('normal', key(k, over), 'other', rows);
  assert.deepEqual(r('i'), { kind: 'mode', mode: 'insert' });
  assert.deepEqual(r('a'), { kind: 'mode', mode: 'insert' });
  assert.deepEqual(r('j'), { kind: 'scroll', scroll: { by: 'lines', n: 1 } });
  assert.deepEqual(r('ArrowDown'), { kind: 'scroll', scroll: { by: 'lines', n: 1 } });
  assert.deepEqual(r('k'), { kind: 'scroll', scroll: { by: 'lines', n: -1 } });
  assert.deepEqual(r('ArrowUp'), { kind: 'scroll', scroll: { by: 'lines', n: -1 } });
  assert.deepEqual(r('G'), { kind: 'scroll', scroll: { by: 'bottom' } });
  assert.deepEqual(r('g'), { kind: 'scroll', scroll: { by: 'top' } });
  assert.deepEqual(r('d', { code: 'KeyD', ctrlKey: true }), { kind: 'scroll', scroll: { by: 'lines', n: 12 } });
  assert.deepEqual(r('u', { code: 'KeyU', ctrlKey: true }, 31), { kind: 'scroll', scroll: { by: 'lines', n: -15 } });
  assert.deepEqual(routeTerminalKey('normal', key('d', { metaKey: true }), 'mac', 24), { kind: 'scroll', scroll: { by: 'lines', n: 12 } });
  assert.deepEqual(r('x'), { kind: 'swallow' }, 'nothing reaches the shell in normal mode');
  assert.deepEqual(r('Escape'), { kind: 'swallow' });
  assert.deepEqual(r('\\', { code: 'Backslash', ctrlKey: true }), { kind: 'toggle' });
  assert.equal(halfPage(1), 1);
  assert.equal(halfPage(0), 1);
});

test('with Vim keys off the terminal has no modal layer', () => {
  for (const mode of ['insert', 'normal']) {
    const r = (k, over, rows = 24) => routeTerminalKey(mode, key(k, over), 'other', rows, false);
    assert.deepEqual(r('Escape'), { kind: 'pty' }, `Esc reaches the shell (${mode})`);
    assert.deepEqual(r('x'), { kind: 'pty' }, `a letter reaches the shell (${mode})`);
    assert.deepEqual(r('i'), { kind: 'pty' });
    assert.deepEqual(r('j'), { kind: 'pty' });
    assert.deepEqual(r('G', { shiftKey: true }), { kind: 'pty' });
    assert.deepEqual(r('d', { code: 'KeyD', ctrlKey: true }), { kind: 'pty' }, 'Ctrl-D is EOF for the shell');
    assert.deepEqual(r('ArrowUp'), { kind: 'pty' }, 'history, not scrollback');
    assert.deepEqual(r('Escape', { isComposing: true }), { kind: 'pty' });
    assert.deepEqual(r('\\', { code: 'Backslash', ctrlKey: true }), { kind: 'toggle' }, 'Ctrl-\\ still hides');
    assert.deepEqual(r('PageUp', { shiftKey: true }), { kind: 'scroll', scroll: { by: 'lines', n: -23 } });
    assert.deepEqual(r('PageDown', { shiftKey: true }), { kind: 'scroll', scroll: { by: 'lines', n: 23 } });
    assert.deepEqual(r('PageUp', { shiftKey: true }, 1), { kind: 'scroll', scroll: { by: 'lines', n: -1 } });
    assert.deepEqual(r('PageUp'), { kind: 'pty' }, 'plain PageUp is the program’s');
  }
  assert.deepEqual(routeTerminalKey('insert', key('c', { metaKey: true }), 'mac', 24, false), { kind: 'native' }, 'Cmd-C still copies');
});

test('the default is the TUI’s: Esc enters normal mode with Vim keys on', () => {
  assert.deepEqual(routeTerminalKey('insert', key('Escape'), 'other', 24, true), { kind: 'mode', mode: 'normal' });
});

test('vim keys stand aside while the terminal (an xterm textarea) has focus', () => {
  const engine = new KeyEngine(desktopKeymap());
  const env = { platform: 'other', target: { tagName: 'TEXTAREA' }, modalOpen: false, now: 0 };
  assert.deepEqual(routeKey(engine, key('j'), env), { kind: 'pass' });
  assert.deepEqual(routeKey(engine, key(':'), env), { kind: 'pass' });
});

// ---------- size ----------

test('measured grids are clamped to what pty.rs accepts', () => {
  assert.deepEqual(clampGrid({ cols: 120.7, rows: 40.2 }), { cols: 120, rows: 40 });
  assert.deepEqual(clampGrid({ cols: 1, rows: 0.5 }), { cols: 2, rows: 1 });
  assert.deepEqual(clampGrid({ cols: 5000, rows: 5000 }), { cols: MAX_COLS, rows: MAX_ROWS });
  assert.equal(clampGrid(undefined), null);
  assert.equal(clampGrid({ cols: 0, rows: 10 }), null);
  assert.equal(clampGrid({ cols: NaN, rows: 10 }), null);
});

test('gridFor divides the pane by the cell size after padding and the scrollbar', () => {
  // (800 - 6 - 14) / 7.8 = 100, (500 - 8) / 16.4 = 30
  assert.deepEqual(gridFor({ width: 800, height: 500 }, { width: 7.8, height: 16.4 }, { padX: 6, padY: 8, scrollbar: 14 }), { cols: 100, rows: 30 });
  assert.deepEqual(gridFor({ width: 801, height: 499 }, { width: 8, height: 16 }), { cols: 100, rows: 31 });
  assert.equal(gridFor({ width: 0, height: 0 }, { width: 8, height: 16 }), null, 'a hidden pane has no grid');
  assert.equal(gridFor({ width: 800, height: 500 }, { width: 0, height: 16 }), null);
});

test('resizeStep only sends a changed grid', () => {
  assert.deepEqual(resizeStep(null, DEFAULT_SIZE), DEFAULT_SIZE);
  assert.equal(resizeStep({ cols: 80, rows: 24 }, { cols: 80, rows: 24 }), null);
  assert.deepEqual(resizeStep({ cols: 80, rows: 24 }, { cols: 81, rows: 24 }), { cols: 81, rows: 24 });
  assert.equal(resizeStep({ cols: 80, rows: 24 }, null), null);
});

test('the overlay is the TUI popup: 90% x 80%, centered', () => {
  assert.deepEqual(overlayRect(1000, 800), { left: 50, top: 80, width: 900, height: 640 });
  assert.deepEqual(overlayRect(10, 4), { left: 0, top: 0, width: 20, height: 5 });
});

test('large pastes are split under the write limit without breaking characters', () => {
  assert.deepEqual(chunkInput(''), []);
  assert.deepEqual(chunkInput('abc'), ['abc']);
  assert.deepEqual(chunkInput('abcdef', 4), ['abcd', 'ef']);
  const parts = chunkInput('a✓✓😀b', 6);
  assert.equal(parts.join(''), 'a✓✓😀b');
  for (const p of parts) assert.ok(new TextEncoder().encode(p).length <= 6);
  assert.deepEqual(parts, ['a✓', '✓', '😀b']);
});

// ---------- registry ----------

test('the terminal is an action with a palette entry, a View menu item and its keys', () => {
  for (const a of TERMINAL_ACTIONS) assert.ok(ACTIONS.some((x) => x.id === a.id), a.id);
  assert.ok(isTerminalAction('toggle-terminal'));
  assert.equal(isTerminalAction('archive'), false);
  assert.deepEqual(keyHints('toggle-terminal'), ['Ctrl-\\', ':term']);
  assert.match(tooltipText('toggle-terminal', 'mac'), /^Terminal \(Cmd-\\, :term\)/);
  const entry = paletteEntries(snap(), 'other').find((e) => e.id === 'toggle-terminal');
  assert.ok(entry?.enabled);
  const view = appMenuModel('other', snap({ terminal: { visible: true, running: true } })).find((m) => m.id === 'view');
  const item = menuItems(view.items).find((n) => n.id === 'act:toggle-terminal');
  assert.equal(item.checked, true);
  assert.equal(item.accelerator, undefined, 'the webview catches Ctrl-\\ itself');
  assert.ok(menuItems(view.items).some((n) => n.id === 'act:kill-terminal'));
});

test(':term, :terminal and :shell open the terminal', () => {
  assert.deepEqual(TERMINAL_COMMAND_BINDINGS, { term: { action: 'toggle-terminal' } });
  const engine = new KeyEngine(desktopKeymap());
  for (const c of ['term', 'terminal', 'shell']) {
    assert.deepEqual(engine.runCommand(c), { kind: 'run', action: 'toggle-terminal', count: null, keys: ':term' }, c);
  }
});

test('ending the shell needs one running; dialogs block both', () => {
  assert.equal(unavailable('toggle-terminal', snap()), null);
  assert.equal(unavailable('toggle-terminal', snap({ open: null })), null, 'no session: the home terminal');
  assert.match(unavailable('kill-terminal', snap()), /No terminal shell/);
  assert.equal(unavailable('kill-terminal', snap({ terminal: { visible: false, running: true } })), null);
  assert.match(unavailable('toggle-terminal', snap({ modalOpen: true })), /Close the open dialog/);
  assert.deepEqual(Object.keys(TERMINAL_RULES).sort(), ['kill-terminal', 'toggle-terminal']);
});

// ---------- bundling ----------

test('xterm.js is vendored into dist/vendor from the pinned packages, no CDN', () => {
  for (const [, name] of VENDORED) assert.ok(existsSync(here(`../dist/vendor/${name}`)), name);
  const js = readFileSync(here('../dist/vendor/xterm.mjs'), 'utf8');
  assert.match(js, /export\s*\{[^}]*\bas Terminal\b/);
  assert.doesNotMatch(js, /sourceMappingURL/);
  assert.match(readFileSync(here('../dist/vendor/addon-fit.mjs'), 'utf8'), /\bas FitAddon\b/);
  const pkg = JSON.parse(readFileSync(here('../package.json'), 'utf8'));
  assert.match(pkg.dependencies['@xterm/xterm'], /^\d+\.\d+\.\d+$/, 'exact pin');
  assert.match(pkg.dependencies['@xterm/addon-fit'], /^\d+\.\d+\.\d+$/, 'exact pin');
  const src = readFileSync(here('../src/actions/terminal.ts'), 'utf8');
  assert.doesNotMatch(src, /https?:\/\//, 'no remote script source');
  assert.equal(stripSourceMap('a\n//# sourceMappingURL=x.map\n'), 'a\n');
  assert.equal(stripSourceMap('a{}\n/*# sourceMappingURL=x.map */'), 'a{}\n');
});

test('the CSP lets xterm style its cells but keeps scripts to the app', () => {
  const conf = JSON.parse(readFileSync(here('../src-tauri/tauri.conf.json'), 'utf8'));
  const csp = conf.app.security.csp;
  assert.match(csp, /style-src 'self' 'unsafe-inline'/);
  assert.match(csp, /script-src 'self';/);
  assert.doesNotMatch(csp.match(/script-src[^;]*/)[0], /unsafe/);
});
