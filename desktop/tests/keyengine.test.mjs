import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  CONFIRM_MS, KeyEngine, buildKeymap, completeCommand, detectPlatform, displayKeys, isTypingTarget, keyToken,
  parseKeys, readVimKeys, resolveCommand, routeKey, writeVimKeys, VIM_KEYS_STORAGE_KEY,
} from '../dist/keyengine.js';
import {
  COMMAND_BINDINGS, KEY_BINDINGS, desktopKeymap, jumpCursor, nextFlagged, stepCursor,
} from '../dist/keybindings.js';
import { COMMANDS, NORMAL_KEYS, RESERVED_KEYS } from '../dist/keymap.generated.js';

const keymap = desktopKeymap();
const fresh = () => new KeyEngine(keymap);
/** Feed a notation string (`5j`, `<Space>gq`); returns every outcome. */
const feedAll = (e, notation, now = 0) => parseKeys(notation).map((t) => e.feed(t, now));
const last = (e, notation, now = 0) => feedAll(e, notation, now).at(-1);
const typeLine = (e, text) => [...text].map((c) => e.feed(c === ' ' ? 'Space' : c));

// ---------- tokens ----------

test('parses keybindings.md notation into tokens and back', () => {
  assert.deepEqual(parseKeys('<Space>gp'), ['Space', 'g', 'p']);
  assert.deepEqual(parseKeys('<Space><Space>'), ['Space', 'Space']);
  assert.deepEqual(parseKeys('gg'), ['g', 'g']);
  assert.deepEqual(parseKeys(']a'), [']', 'a']);
  assert.deepEqual(parseKeys('Ctrl-Alt-G'), ['C-A-g']);
  assert.deepEqual(parseKeys('Ctrl-N'), ['C-n']);
  assert.deepEqual(parseKeys('Ctrl-Shift-Tab'), ['C-S-Tab']);
  assert.deepEqual(parseKeys('Enter'), ['Enter']);
  assert.deepEqual(parseKeys('F3'), ['F3']);
  assert.deepEqual(parseKeys('-'), ['-']);
  assert.equal(displayKeys(parseKeys('<Space>gp')), '<Space>gp');
  assert.equal(displayKeys(parseKeys('Ctrl-Alt-G')), 'Ctrl-Alt-G');
});

test('keyboard events map to tokens the same way on Linux, macOS and Windows', () => {
  for (const p of ['mac', 'other']) {
    assert.equal(keyToken({ key: 'j', code: 'KeyJ' }, p), 'j');
    assert.equal(keyToken({ key: 'G', code: 'KeyG', shiftKey: true }, p), 'G');
    assert.equal(keyToken({ key: ':', code: 'Semicolon', shiftKey: true }, p), ':');
    assert.equal(keyToken({ key: ' ', code: 'Space' }, p), 'Space');
    assert.equal(keyToken({ key: 'Escape', code: 'Escape' }, p), 'Esc');
    assert.equal(keyToken({ key: 'Tab', shiftKey: true }, p), 'S-Tab');
    assert.equal(keyToken({ key: 'n', code: 'KeyN', ctrlKey: true }, p), 'C-n');
    assert.equal(keyToken({ key: 'F3', code: 'F3' }, p), 'F3');
    assert.equal(keyToken({ key: 'Shift', code: 'ShiftLeft', shiftKey: true }, p), null);
    assert.equal(keyToken({ key: 'g', code: 'KeyG', isComposing: true }, p), null);
    assert.equal(keyToken({ key: 'Dead', code: 'Quote' }, p), null);
  }
  // macOS Option turns g into a glyph; the physical code still names the key.
  assert.equal(keyToken({ key: '©', code: 'KeyG', ctrlKey: true, altKey: true }, 'mac'), 'C-A-g');
  assert.equal(keyToken({ key: 'g', code: 'KeyG', ctrlKey: true, altKey: true }, 'other'), 'C-A-g');
});

test('Cmd acts as Ctrl on macOS but system chords stay native; Super is never ours', () => {
  assert.equal(keyToken({ key: 'n', code: 'KeyN', metaKey: true }, 'mac'), 'C-n');
  for (const k of ['c', 'v', 'x', 'a', 'z', 'q', 'w', 'h', 'm']) {
    assert.equal(keyToken({ key: k, code: `Key${k.toUpperCase()}`, metaKey: true }, 'mac'), null, `Cmd-${k}`);
  }
  assert.equal(keyToken({ key: 'n', code: 'KeyN', metaKey: true }, 'other'), null);
  for (const k of ['c', 'v', 'x', 'a', 'z', 'y']) {
    assert.equal(keyToken({ key: k, code: `Key${k.toUpperCase()}`, ctrlKey: true }, 'other'), null, `Ctrl-${k}`);
  }
  // On macOS physical Ctrl-C is free for the engine (it is simply unbound).
  assert.equal(keyToken({ key: 'c', code: 'KeyC', ctrlKey: true }, 'mac'), 'C-c');
  assert.equal(detectPlatform('MacIntel'), 'mac');
  assert.equal(detectPlatform('macOS'), 'mac');
  assert.equal(detectPlatform('Linux x86_64'), 'other');
  assert.equal(detectPlatform('Win32'), 'other');
});

// ---------- normal mode ----------

test('single keys run their desktop action without a count', () => {
  const e = fresh();
  assert.deepEqual(e.feed('j'), { kind: 'run', action: 'cursor-down', count: null, keys: 'j' });
  assert.deepEqual(e.feed('k'), { kind: 'run', action: 'cursor-up', count: null, keys: 'k' });
  assert.deepEqual(e.feed('G'), { kind: 'run', action: 'cursor-last', count: null, keys: 'G' });
  assert.equal(e.feed('Enter').action, 'open');
  assert.equal(e.feed('x').action, 'soft-interrupt');
  assert.equal(e.feed('/').action, 'focus-search');
  assert.equal(e.feed('C-n').action, 'launch');
  assert.equal(e.busy(), false);
});

test('numeric counts prefix a motion and are capped', () => {
  const e = fresh();
  assert.deepEqual(feedAll(e, '5j').map((o) => o.kind), ['pending', 'run']);
  assert.equal(last(e, '5j').count, 5);
  assert.equal(last(e, '12G').count, 12);
  assert.equal(last(e, '10k').count, 10);
  assert.equal(last(e, '3gg').count, 3);
  assert.equal(last(e, '99999j').count, 9999);
  assert.equal(e.view().pending, '');
  // A lone 0 is not a count.
  assert.deepEqual(e.feed('0'), { kind: 'unbound', keys: '0' });
});

test('the count shows while pending and Esc drops it', () => {
  const e = fresh();
  e.feed('4');
  assert.equal(e.view().pending, '4');
  assert.deepEqual(e.feed('Esc'), { kind: 'cancel' });
  assert.equal(e.feed('j').count, null);
});

test('g and z chords wait for their second key', () => {
  const e = fresh();
  assert.deepEqual(e.feed('g'), { kind: 'pending' });
  assert.equal(e.view().pending, 'g');
  assert.deepEqual(e.feed('g'), { kind: 'run', action: 'cursor-first', count: null, keys: 'gg' });
  assert.equal(last(e, 'ga').action, 'view-archived');
  assert.equal(last(e, 'gs').action, 'view-active');
  assert.deepEqual(last(e, 'zo'), { kind: 'run', action: 'fold-open', count: null, keys: 'zo' });
  assert.deepEqual(last(e, 'zq'), { kind: 'unbound', keys: 'zq' });
  assert.deepEqual(last(e, ']a'), { kind: 'run', action: 'next-attention', count: null, keys: ']a' });
  assert.equal(last(e, '[a').action, 'prev-attention');
  assert.deepEqual(last(e, 'DD'), { kind: 'run', action: 'delete-session', count: null, keys: 'DD' });
});

test('the Space leader tree resolves nested chords and shows which-key hints', () => {
  const e = fresh();
  assert.deepEqual(e.feed('Space'), { kind: 'pending' });
  const hints = e.view().hints;
  const byKey = Object.fromEntries(hints.map((h) => [h.key, h]));
  assert.deepEqual(byKey.a, { key: 'a', title: 'Archive selected session', bound: true, prefix: false });
  assert.deepEqual(byKey.i, { key: 'i', title: 'Open or focus Issues workspace', bound: true, prefix: false });
  assert.equal(byKey.g.prefix, true);
  assert.equal(byKey.g.bound, true, '<Space>gq is bound below <Space>g');
  assert.equal(byKey['<Space>'].title, 'Find file');
  assert.equal(e.view().pending, '<Space>');
  e.feed('g');
  const g = Object.fromEntries(e.view().hints.map((h) => [h.key, h]));
  assert.equal(g.q.bound, true);
  assert.equal(g.P.bound, true, '<Space>gP is the prompt creator (local #88)');
  assert.equal(g.r, undefined, 'retired <Space>gr is not offered');
  assert.deepEqual(e.feed('q'), { kind: 'run', action: 'answer-question', count: null, keys: '<Space>gq' });
  assert.deepEqual(e.view().hints, []);
  assert.deepEqual(last(e, '<Space>gP'), { kind: 'run', action: 'prompt-creator', count: null, keys: '<Space>gP' });
  assert.equal(last(e, '<Space>a').action, 'archive');
  assert.equal(last(e, '<Space>c').action, 'quick-continue');
  assert.equal(last(e, '<Space>N').action, 'launch');
  assert.equal(last(e, '<Space>w').kind, 'unbound');
  assert.deepEqual(last(e, '<Space>w'), { kind: 'unbound', keys: '<Space>w' });
});

test('digits after the leader are keys, not counts', () => {
  const e = fresh();
  assert.deepEqual(last(e, '<Space>3'), { kind: 'run', action: 'attention-slot', count: null, keys: '<Space>3', arg: '3' });
  assert.deepEqual(last(e, 'gf2'), { kind: 'run', action: 'open-recent-file', count: null, keys: 'gf2', arg: '2' });
});

test('Esc cancels a pending chord; idle Esc and unknown named keys pass to the webview', () => {
  const e = fresh();
  e.feed('Space');
  e.feed('g');
  assert.deepEqual(e.feed('Esc'), { kind: 'cancel' });
  assert.equal(e.busy(), false);
  assert.deepEqual(e.feed('Esc'), { kind: 'pass' });
  assert.deepEqual(e.feed('Tab'), { kind: 'pass' });
  assert.deepEqual(e.feed('C-r'), { kind: 'pass' });
  assert.deepEqual(e.feed('w'), { kind: 'unbound', keys: 'w' });
});

test('retired TUI chords explain themselves', () => {
  const e = fresh();
  assert.deepEqual(last(e, 'gt'), { kind: 'retired', keys: 'gt', why: 'retired TaskRabbit zone chord; use gs / gj / ga' });
});

test('X and <Space>hc need a second press within five seconds', () => {
  const e = fresh();
  assert.deepEqual(e.feed('X', 1000), { kind: 'confirm', action: 'hard-pause', keys: 'X', title: 'INTERRUPT NOW' });
  assert.deepEqual(e.feed('X', 1000 + CONFIRM_MS), { kind: 'run', action: 'hard-pause', count: null, keys: 'X' });
  assert.equal(e.feed('X', 20_000).kind, 'confirm');
  assert.equal(e.feed('X', 20_000 + CONFIRM_MS + 1).kind, 'confirm', 'too late: arms again');
  // Another key in between disarms.
  e.feed('X', 40_000);
  e.feed('j', 40_001);
  assert.equal(e.feed('X', 40_002).kind, 'confirm');
  assert.equal(last(e, '<Space>hc', 50_000).kind, 'confirm');
  assert.equal(last(e, '<Space>hc', 50_001).action, 'clear-pause');
});

test('every TUI Normal-mode key gives an action, a confirm or a notice, never silence', () => {
  for (const row of NORMAL_KEYS) {
    const o = last(fresh(), row.keys);
    assert.ok(['run', 'confirm', 'missing'].includes(o.kind), `${row.keys} -> ${o.kind}`);
    if (o.kind === 'missing') assert.equal(o.title, row.title);
    const bound = KEY_BINDINGS[row.keys];
    assert.equal(o.kind !== 'missing', !!bound, row.keys);
  }
});

test('every desktop binding names a documented TUI key or command', () => {
  const keys = new Set(NORMAL_KEYS.map((r) => r.keys));
  for (const k of Object.keys(KEY_BINDINGS)) assert.ok(keys.has(k), k);
  const names = new Set(COMMANDS.map((c) => c.name));
  for (const c of Object.keys(COMMAND_BINDINGS)) assert.ok(names.has(c), c);
  assert.throws(
    () => buildKeymap({ normal: NORMAL_KEYS, reserved: RESERVED_KEYS, commands: COMMANDS, keyBindings: { zz9: { action: 'x' } }, commandBindings: {} }),
    /desktop binds zz9/,
  );
  assert.throws(
    () => buildKeymap({ normal: NORMAL_KEYS, reserved: RESERVED_KEYS, commands: COMMANDS, keyBindings: {}, commandBindings: { nope: { action: 'x' } } }),
    /desktop binds :nope/,
  );
});

test('a key that is also a prefix of another key is rejected', () => {
  assert.throws(() => buildKeymap({
    normal: [{ keys: 'g', title: 'a', description: '' }, { keys: 'gg', title: 'b', description: '' }],
    reserved: [], commands: [], keyBindings: {}, commandBindings: {},
  }), /overlaps/);
});

// ---------- command line ----------

test(': opens the command line; typing, Backspace and Enter run a command', () => {
  const e = fresh();
  assert.deepEqual(e.feed(':'), { kind: 'command' });
  assert.equal(e.view().mode, 'command');
  typeLine(e, 'archivx');
  e.feed('Backspace');
  e.feed('e');
  assert.equal(e.view().command.text, 'archive');
  assert.deepEqual(e.feed('Enter'), { kind: 'run', action: 'archive', count: null, keys: ':archive' });
  assert.equal(e.view().mode, 'normal');
});

test('commands keep their aliases and arguments', () => {
  const run = (line) => { const e = fresh(); e.feed(':'); typeLine(e, line); return e.feed('Enter'); };
  assert.equal(run('arc').action, 'archive');
  assert.deepEqual(run('continue hello world'), { kind: 'run', action: 'continue', count: null, keys: ':continue', arg: 'hello world' });
  assert.deepEqual(run('cont'), { kind: 'run', action: 'continue', count: null, keys: ':continue' });
  assert.equal(run('pause clear').action, 'clear-pause');
  assert.equal(run('ki').action, 'soft-interrupt');
  assert.deepEqual(run('blank fix the tests'), { kind: 'run', action: 'launch', count: null, keys: ':blank', arg: 'fix the tests' });
  assert.deepEqual(run('theme'), { kind: 'run', action: 'theme-picker', count: null, keys: ':theme' });
  assert.deepEqual(run('theme ruby'), { kind: 'run', action: 'theme-picker', count: null, keys: ':theme', arg: 'ruby' });
  assert.deepEqual(run('q'), { kind: 'run', action: 'quit', count: null, keys: ':quit' });
  assert.deepEqual(run('del'), { kind: 'run', action: 'delete-session', count: null, keys: ':delete' });
  // G3 (hub #3000) bound the last unbound TUI command; :manager restart now runs.
  assert.deepEqual(run('manager restart now'), { kind: 'run', action: 'mgr-restart', count: null, keys: ':manager restart', arg: 'now' });
  assert.deepEqual(run('frobnicate'), { kind: 'error', message: 'Not an rsi command: :frobnicate' });
  assert.deepEqual(run('project-delete'), { kind: 'error', message: ':project-delete needs an argument' });
  assert.deepEqual(run('archive now'), { kind: 'error', message: ':archive takes no argument' });
});

test('the longest command name wins', () => {
  const r = resolveCommand(COMMANDS, 'manager global appoint alpha,beta');
  assert.equal(r.row.name, 'manager global appoint');
  assert.equal(r.arg, 'alpha,beta');
  assert.equal(resolveCommand(COMMANDS, ':manager global').row.name, 'manager global');
  assert.equal(resolveCommand(COMMANDS, 'manager workspace').row.name, 'global-manager');
});

test('Tab cycles completions, Shift-Tab goes back, typing restarts them', () => {
  const e = fresh();
  e.feed(':');
  typeLine(e, 'arch');
  assert.deepEqual(e.view().command.completions, ['archive', 'archives']);
  e.feed('Tab');
  assert.equal(e.view().command.text, 'archive');
  assert.equal(e.view().command.selected, 0);
  e.feed('Tab');
  assert.equal(e.view().command.text, 'archives');
  e.feed('Tab');
  assert.equal(e.view().command.text, 'archive', 'wraps');
  e.feed('S-Tab');
  assert.equal(e.view().command.text, 'archives');
  assert.equal(e.feed('Enter').action, 'view-archived');
  e.feed(':');
  typeLine(e, 'pause ');
  assert.deepEqual(e.view().command.completions, ['pause soft', 'pause clear']);
  e.feed('C-n');
  e.feed('C-n');
  assert.equal(e.view().command.text, 'pause clear');
  e.feed('Backspace');
  assert.equal(e.view().command.selected, -1);
  assert.equal(e.view().command.text, 'pause clea');
  assert.deepEqual(e.view().command.completions, ['pause clear']);
});

test('completion lists names before aliases', () => {
  assert.deepEqual(completeCommand(COMMANDS, 'q'), ['quit', 'qall', 'qa', 'quit!', 'q!']);
});

test('Esc or Backspace on an empty line closes the command line; Enter on empty does nothing', () => {
  const e = fresh();
  e.feed(':');
  typeLine(e, 'ar');
  assert.deepEqual(e.feed('Esc'), { kind: 'cancel' });
  assert.equal(e.view().mode, 'normal');
  e.feed(':');
  assert.deepEqual(e.feed('Backspace'), { kind: 'cancel' });
  e.feed(':');
  assert.deepEqual(e.feed('Enter'), { kind: 'cancel' });
});

test('keys typed on the command line never run Normal-mode bindings', () => {
  const e = fresh();
  e.feed(':');
  for (const t of ['j', 'X', 'g', 'g', 'Space', 'x']) assert.deepEqual(e.feed(t), { kind: 'command' });
  assert.equal(e.view().command.text, 'jXgg x');
});

test('<Space>; opens the palette; the : line also opens through the engine', () => {
  const e = fresh();
  assert.equal(last(e, '<Space>;').action, 'palette');
  assert.deepEqual(e.openCommandLine(), { kind: 'command' });
  assert.equal(e.view().mode, 'command');
  assert.deepEqual(e.insertText('arch\nive'), { kind: 'command' });
  assert.equal(e.view().command.text, 'arch ive');
});

// ---------- routing: text fields, dialogs, toggle ----------

const env = (target, extra = {}) => ({ platform: 'other', target, modalOpen: false, now: 0, ...extra });
const ev = (key, extra = {}) => ({ key, code: /^[a-z]$/.test(key) ? `Key${key.toUpperCase()}` : '', ...extra });

test('typing in a text field never triggers a binding', () => {
  const targets = [
    { tagName: 'INPUT' }, { tagName: 'INPUT', type: 'search' }, { tagName: 'INPUT', type: 'text' },
    { tagName: 'TEXTAREA' }, { tagName: 'SELECT' }, { tagName: 'input' },
    { tagName: 'DIV', isContentEditable: true },
  ];
  for (const t of targets) {
    assert.equal(isTypingTarget(t), true);
    const e = fresh();
    for (const k of ['j', 'g', 'g', ' ', 'X', 'X', ':', 'Escape', 'Enter']) {
      assert.deepEqual(routeKey(e, ev(k), env(t)), { kind: 'pass' }, `${t.tagName} ${k}`);
    }
    assert.equal(e.busy(), false);
  }
  assert.equal(isTypingTarget({ tagName: 'BODY' }), false);
  assert.equal(isTypingTarget({ tagName: 'INPUT', type: 'checkbox' }), false, 'a toggled checkbox keeps the keys');
  assert.equal(isTypingTarget(null), false);
});

test('a chord started outside a text field never completes inside one', () => {
  const e = fresh();
  assert.deepEqual(routeKey(e, ev('g'), env({ tagName: 'BODY' })), { kind: 'pending' });
  assert.deepEqual(routeKey(e, ev('g'), env({ tagName: 'INPUT' })), { kind: 'pass' });
});

test('keys reach the engine from the page, and pass while a modal dialog is open', () => {
  const e = fresh();
  assert.equal(routeKey(e, ev('j'), env({ tagName: 'BODY' })).action, 'cursor-down');
  assert.deepEqual(routeKey(e, ev('j'), env({ tagName: 'BODY' }, { modalOpen: true })), { kind: 'pass' });
  assert.deepEqual(routeKey(e, ev('j', { defaultPrevented: true }), env({ tagName: 'BODY' })), { kind: 'pass' });
  assert.deepEqual(routeKey(e, ev('c', { ctrlKey: true }), env({ tagName: 'BODY' })), { kind: 'pass' }, 'copy stays native');
});

test('Enter on a focused button keeps its click unless a command line is open', () => {
  const e = fresh();
  const btn = { tagName: 'BUTTON' };
  assert.deepEqual(routeKey(e, ev('Enter'), env(btn)), { kind: 'pass' });
  routeKey(e, ev(':'), env(btn));
  for (const c of 'archive') routeKey(e, ev(c), env(btn));
  assert.equal(routeKey(e, ev('Enter'), env(btn)).action, 'archive');
});

test('turning Vim keys off disables every binding and drops pending keys', () => {
  const e = fresh();
  e.feed('Space');
  e.feed(':');
  e.setEnabled(false);
  assert.equal(e.busy(), false);
  for (const row of NORMAL_KEYS) {
    for (const t of parseKeys(row.keys)) assert.deepEqual(e.feed(t), { kind: 'pass' }, row.keys);
  }
  assert.deepEqual(e.feed(':'), { kind: 'pass' });
  assert.deepEqual(e.openCommandLine(), { kind: 'pass' });
  assert.deepEqual(e.insertText('x'), { kind: 'pass' });
  assert.deepEqual(routeKey(e, ev('j'), env({ tagName: 'BODY' })), { kind: 'pass' });
  e.setEnabled(true);
  assert.equal(e.feed('j').action, 'cursor-down');
});

test('the Vim keys setting persists and defaults to on', () => {
  const mem = new Map();
  const store = { getItem: (k) => mem.get(k) ?? null, setItem: (k, v) => mem.set(k, v) };
  assert.equal(readVimKeys(store), true);
  writeVimKeys(store, false);
  assert.equal(mem.get(VIM_KEYS_STORAGE_KEY), 'off');
  assert.equal(readVimKeys(store), false);
  writeVimKeys(store, true);
  assert.equal(readVimKeys(store), true);
  const broken = { getItem: () => { throw new Error('denied'); }, setItem: () => { throw new Error('denied'); } };
  assert.equal(readVimKeys(broken), true);
  assert.doesNotThrow(() => writeVimKeys(broken, false));
  assert.equal(readVimKeys(undefined), true);
});

// ---------- cursor helpers ----------

test('cursor motions clamp, start from the ends and honour counts', () => {
  const ids = ['a', 'b', 'c', 'd'];
  assert.equal(stepCursor(ids, null, 1), 'a');
  assert.equal(stepCursor(ids, null, -1), 'd');
  assert.equal(stepCursor(ids, 'b', 1), 'c');
  assert.equal(stepCursor(ids, 'b', 5), 'd');
  assert.equal(stepCursor(ids, 'b', -5), 'a');
  assert.equal(stepCursor(ids, 'gone', 2), 'b');
  assert.equal(stepCursor([], 'a', 1), null);
  assert.equal(jumpCursor(ids, null, 'first'), 'a');
  assert.equal(jumpCursor(ids, null, 'last'), 'd');
  assert.equal(jumpCursor(ids, 2, 'last'), 'b');
  assert.equal(jumpCursor(ids, 40, 'first'), 'd');
  assert.equal(jumpCursor([], null, 'first'), null);
});

test(']a and [a find the next session needing attention, wrapping', () => {
  const ids = ['a', 'b', 'c', 'd'];
  const flags = [false, true, false, true];
  assert.equal(nextFlagged(ids, flags, 'b', 1), 'd');
  assert.equal(nextFlagged(ids, flags, 'd', 1), 'b');
  assert.equal(nextFlagged(ids, flags, 'b', -1), 'd');
  assert.equal(nextFlagged(ids, flags, null, 1), 'b');
  assert.equal(nextFlagged(ids, flags, null, -1), 'd');
  assert.equal(nextFlagged(ids, [false, false, false, false], 'a', 1), null);
});
