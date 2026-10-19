// Project files (slice 10B, hub #3012): the tree model, the picker's scoring
// order, the git gutter mapping, the viewer's paging and keys, calm refusals,
// the registry wiring and the request shapes the webview sends.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  FileTree, NO_ROOT, NO_VIEWER_KEYS, absolutePath, canReadMore, chooseRoot, drawerWidth, dropSegment, entryInfo, fileErrorText,
  fuzzyMatch, gutterNote, gutterSigns, moveView, movePickerSelection, pickerKey, rankPaths, readSummary, settle,
  splitLines, treeKey, viewerCommand, viewerKey,
} from '../dist/files-model.js';
import { FILES_ACTIONS, FILES_KEY_BINDINGS, FILES_RULES, SLICE, isFilesAction } from '../dist/actions/files.js';
import { ACTIONS, keyHints, sliceProblems, unavailable } from '../dist/registry.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';

const P = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';
const S = '11111111-1111-4111-8111-111111111111';

const entry = (path, kind = 'file', over = {}) => {
  const name = path.split('/').pop();
  return { name, path, kind, size: 10, hidden: name.startsWith('.'), ignored: false, ...over };
};
const listing = (...entries) => ({ entries, truncated: false });
const names = (t) => t.rows().map((r) => (r.kind === 'entry' ? `${'  '.repeat(r.depth)}${r.entry.name}${r.expanded ? '/' : ''}` : `${'  '.repeat(r.depth)}[${r.tone}]`));

function rootTree() {
  const t = new FileTree();
  t.setLoading('');
  t.setListing('', listing(
    entry('src', 'dir'), entry('README.md'), entry('.github', 'dir'), entry('.env'),
    entry('target', 'dir', { ignored: true }), entry('docs', 'dir'),
  ));
  return t;
}

// ---------- tree ----------

test('the tree sorts dirs first, hides dotfiles until toggled and keeps ignored entries flagged', () => {
  const t = new FileTree();
  t.setLoading('');
  assert.deepEqual(names(t), ['[loading]']);
  t.setListing('', listing(entry('src', 'dir'), entry('README.md'), entry('.github', 'dir'), entry('.env'),
    entry('target', 'dir', { ignored: true }), entry('docs', 'dir')));
  assert.deepEqual(names(t), ['docs', 'src', 'target', 'README.md']);
  assert.equal(t.selected, 'docs', 'the first entry is selected once the root loads');
  const target = t.entries().find((r) => r.entry.path === 'target');
  assert.equal(target.entry.ignored, true, 'ignored entries stay, flagged for dimming');
  t.toggleHidden();
  assert.deepEqual(names(t), ['.github', 'docs', 'src', 'target', '.env', 'README.md']);
  t.selected = '.env';
  t.toggleHidden();
  assert.equal(t.selected, 'docs', 'a hidden selection moves to a visible entry');
});

test('directories load lazily and l / h / Enter follow the TUI', () => {
  const t = rootTree();
  t.selected = 'src';
  assert.deepEqual(t.openNode(), { kind: 'load', path: 'src' }, 'l on a collapsed dir expands and asks for its listing');
  assert.deepEqual(names(t), ['docs', 'src/', '  [loading]', 'target', 'README.md']);
  t.setListing('src', listing(entry('src/main.rs'), entry('src/ui', 'dir')));
  assert.deepEqual(names(t), ['docs', 'src/', '  ui', '  main.rs', 'target', 'README.md']);
  assert.deepEqual(t.openNode(), { kind: 'none' });
  assert.equal(t.selected, 'src/ui', 'l on an expanded dir steps into its first child');
  t.move(1);
  assert.deepEqual(t.openNode(), { kind: 'open', path: 'src/main.rs' }, 'l on a file opens it');
  t.closeNode();
  assert.equal(t.selected, 'src', 'h on a file collapses the parent and selects it');
  assert.deepEqual(names(t), ['docs', 'src', 'target', 'README.md']);
  assert.deepEqual(t.activate(), { kind: 'none' }, 'Enter re-expands a dir already listed without a new load');
  assert.deepEqual(names(t), ['docs', 'src/', '  ui', '  main.rs', 'target', 'README.md']);
  t.activate();
  assert.deepEqual(names(t), ['docs', 'src', 'target', 'README.md'], 'Enter on an expanded dir collapses it');
  t.selected = 'README.md';
  assert.deepEqual(t.activate(), { kind: 'open', path: 'README.md' });
});

test('moves clamp, g / G jump, and z collapses to the top-level ancestor', () => {
  const t = rootTree();
  t.move(-5);
  assert.equal(t.selected, 'docs');
  t.move(Infinity);
  assert.equal(t.selected, 'README.md');
  t.move(-Infinity);
  assert.equal(t.selected, 'docs');
  t.selected = 'src';
  t.openNode();
  t.setListing('src', listing(entry('src/ui', 'dir')));
  t.selected = 'src/ui';
  t.openNode();
  t.setListing('src/ui', listing(entry('src/ui/view.ts')));
  t.selected = 'src/ui/view.ts';
  assert.deepEqual(t.loadedDirs(), ['', 'src', 'src/ui'], 'a refresh re-reads the root and the open dirs');
  t.collapseAll();
  assert.equal(t.selected, 'src');
  assert.deepEqual(names(t), ['docs', 'src', 'target', 'README.md']);
});

test('reveal expands the ancestors of a file, asks only for unlisted ones and shows hidden ones', () => {
  const t = rootTree();
  t.selected = 'src';
  t.openNode();
  t.setListing('src', listing(entry('src/ui', 'dir')));
  assert.deepEqual(t.reveal('src/ui/deep/view.ts'), ['src/ui', 'src/ui/deep']);
  assert.equal(t.selected, 'src/ui/deep/view.ts');
  const h = rootTree();
  assert.deepEqual(h.reveal('.github/workflows/ci.yml'), ['.github', '.github/workflows']);
  assert.equal(h.showHidden, true);
});

test('a listing error, a capped listing and an empty folder show as notes, not entries', () => {
  const t = rootTree();
  t.selected = 'docs';
  t.openNode();
  t.setError('docs', 'That file or folder is no longer there. Refresh the tree (R).');
  t.selected = 'src';
  t.openNode();
  t.setListing('src', { entries: [entry('src/a.rs')], truncated: true });
  t.selected = 'target';
  t.openNode();
  t.setListing('target', listing(entry('target/.cache', 'dir')));
  assert.deepEqual(names(t), ['docs/', '  [error]', 'src/', '  a.rs', '  [truncated]', 'target/', '  [empty]', 'README.md']);
  const empty = t.rows().find((r) => r.tone === 'empty');
  assert.equal(empty.text, '1 hidden (. shows them)');
  assert.deepEqual(t.entries().map((r) => r.entry.path), ['docs', 'src', 'src/a.rs', 'target', 'README.md']);
});

// ---------- picker ----------

const FEED = [
  'docs/keybindings.md', 'crates/rsi/src/main.rs', 'desktop/src/main.ts', 'crates/rsid/src/rpc/mod.rs',
  'src/domain_main.ts', 'README.md', 'desktop/src/files-model.ts', 'main.ts', 'crates/rsi/src/ui/file_renderer.rs',
];
const ranked = (q) => rankPaths(FEED, q).map((r) => r.path);

test('the picker scorer pins its ordering: exact and shorter, word-start and consecutive matches first', () => {
  assert.deepEqual(ranked('main'), ['main.ts', 'desktop/src/main.ts', 'crates/rsi/src/main.rs', 'src/domain_main.ts']);
  assert.deepEqual(ranked('mrs'), ['crates/rsi/src/main.rs', 'crates/rsid/src/rpc/mod.rs']);
  assert.deepEqual(ranked('dsm'), ['desktop/src/main.ts', 'desktop/src/files-model.ts', 'crates/rsid/src/rpc/mod.rs', 'docs/keybindings.md']);
  assert.deepEqual(ranked('fm'), ['desktop/src/files-model.ts']);
  assert.deepEqual(ranked('readme'), ['README.md'], 'lower-case queries ignore case');
  assert.deepEqual(ranked('FM'), [], 'a capital in the query matches case-sensitively (smart case)');
  assert.deepEqual(ranked('zzz'), []);
  assert.deepEqual(ranked(''), FEED, 'an empty query lists the feed in order');
  assert.deepEqual(ranked('ma in'), ranked('main'), 'spaces in the query are ignored');
});

test('the scorer reports the matched characters for highlighting and caps the result list', () => {
  assert.deepEqual(fuzzyMatch('crates/rsi/src/main.rs', 'mrs').hits, [15, 20, 21]);
  assert.deepEqual(fuzzyMatch('main.ts', 'main.ts'), { score: Infinity, hits: [0, 1, 2, 3, 4, 5, 6] });
  assert.equal(fuzzyMatch('abc', 'abd'), null);
  const many = Array.from({ length: 250 }, (_, i) => `f${i}.txt`);
  assert.equal(rankPaths(many, 'f').length, 100);
  assert.equal(rankPaths(many, 'f', 5).length, 5);
  assert.deepEqual(rankPaths(['b/x', 'a/x'], 'x').map((r) => r.index), [0, 1], 'ties keep the feed order');
  assert.equal(movePickerSelection(0, 3, -1), 0);
  assert.equal(movePickerSelection(2, 3, 1), 2);
  assert.equal(movePickerSelection(0, 0, 1), 0);
});

test('picker keys: arrows and Ctrl-j/k/n/p move, Ctrl-u clears, Enter opens, Esc closes, j and k type', () => {
  assert.deepEqual(pickerKey({ key: 'ArrowDown' }), { kind: 'move', n: 1 });
  assert.deepEqual(pickerKey({ key: 'j', ctrlKey: true }), { kind: 'move', n: 1 });
  assert.deepEqual(pickerKey({ key: 'p', ctrlKey: true }), { kind: 'move', n: -1 });
  assert.deepEqual(pickerKey({ key: 'u', ctrlKey: true }), { kind: 'clear' });
  assert.deepEqual(pickerKey({ key: 'w', ctrlKey: true }), { kind: 'clear-segment' });
  assert.equal(dropSegment('src/ui/vie'), 'src/ui/');
  assert.equal(dropSegment('src/ui/'), 'src/');
  assert.equal(dropSegment('main'), '');
  assert.deepEqual(pickerKey({ key: 'Enter' }), { kind: 'open' });
  assert.deepEqual(pickerKey({ key: 'Escape' }), { kind: 'close' });
  assert.deepEqual(pickerKey({ key: 'j' }), { kind: 'type' });
  assert.deepEqual(pickerKey({ key: 'k' }), { kind: 'type' });
});

// ---------- gutter ----------

test('the gutter maps rsid markers to the TUI signs', () => {
  const signs = gutterSigns([
    { line: 1, kind: 'modified' }, { line: 1, kind: 'removed' },
    { line: 3, kind: 'added' }, { line: 4, kind: 'added' },
    { line: 6, kind: 'removed' },
    { line: 99, kind: 'added' },
  ], 8);
  assert.deepEqual(signs, ['modified', null, 'added', 'added', null, 'removed', null, null]);
  assert.deepEqual(gutterSigns([{ line: 0, kind: 'removed' }], 3), ['removed', null, null], 'a deletion before line 1 marks line 1');
  assert.deepEqual(gutterSigns([{ line: 3, kind: 'removed' }], 3), [null, null, 'removed'], 'a deletion at the end marks the last line');
  assert.deepEqual(gutterSigns([{ line: 2, kind: 'removed' }, { line: 2, kind: 'added' }], 2), [null, 'added'], 'added wins whatever the order');
  assert.deepEqual(gutterSigns([{ line: 0, kind: 'removed' }], 0), [], 'an empty file has no rows to mark');
  assert.equal(gutterNote('tracked'), null);
  assert.equal(gutterNote('not_a_repo'), 'not in a git repository');
  assert.equal(gutterNote('ignored'), 'ignored by git');
});

// ---------- viewer ----------

test('viewer paging follows the TUI keys and keeps the cursor on screen', () => {
  const lines = 100;
  const rows = 20;
  let p = { cursor: 0, top: 0 };
  p = moveView(p, lines, rows, { by: 'line', n: 25 });
  assert.deepEqual(p, { cursor: 25, top: 6 });
  p = moveView(p, lines, rows, { by: 'half', n: 1 });
  assert.deepEqual(p, { cursor: 35, top: 16 });
  p = moveView(p, lines, rows, { by: 'page', n: 1 });
  assert.deepEqual(p, { cursor: 55, top: 36 });
  p = moveView(p, lines, rows, { by: 'page', n: 5 });
  assert.deepEqual(p, { cursor: 99, top: 80 });
  p = moveView(p, lines, rows, { by: 'half', n: -1 });
  assert.deepEqual(p, { cursor: 89, top: 70 });
  assert.deepEqual(moveView(p, lines, rows, { by: 'top' }), { cursor: 0, top: 0 });
  assert.deepEqual(moveView(p, lines, rows, { by: 'bottom' }), { cursor: 99, top: 80 });
  assert.deepEqual(moveView(p, lines, rows, { by: 'goto', line: 50 }), { cursor: 49, top: 39 });
  assert.deepEqual(moveView(p, 5, rows, { by: 'goto', line: 50 }), { cursor: 4, top: 0 });
  assert.deepEqual(settle({ cursor: 3, top: 40 }, 0, rows), { cursor: 0, top: 0 });
});

test('viewer keys: counts, gg, the Space leader, edit keys and passing chords', () => {
  const run = (keys) => {
    let st = NO_VIEWER_KEYS;
    let cmd;
    for (const k of keys) ({ cmd, next: st } = viewerKey(st, typeof k === 'string' ? { key: k } : k));
    return cmd;
  };
  assert.deepEqual(run(['j']), { kind: 'move', move: { by: 'line', n: 1 } });
  assert.deepEqual(run(['5', 'k']), { kind: 'move', move: { by: 'line', n: -5 } });
  assert.deepEqual(run(['g', 'g']), { kind: 'move', move: { by: 'top' } });
  assert.deepEqual(run(['4', '2', 'G']), { kind: 'move', move: { by: 'goto', line: 42 } });
  assert.deepEqual(run(['G']), { kind: 'move', move: { by: 'bottom' } });
  assert.deepEqual(run([{ key: 'd', ctrlKey: true }]), { kind: 'move', move: { by: 'half', n: 1 } });
  assert.deepEqual(run([{ key: 'b', ctrlKey: true }]), { kind: 'move', move: { by: 'page', n: -1 } });
  assert.deepEqual(run(['PageDown']), { kind: 'move', move: { by: 'page', n: 1 } });
  assert.deepEqual(run([' ', 'q']), { kind: 'close' });
  assert.deepEqual(run([' ', 'e']), { kind: 'explorer' });
  assert.deepEqual(run([' ', ' ']), { kind: 'picker' });
  assert.deepEqual(run(['q']), { kind: 'close' });
  assert.deepEqual(run(['Backspace']), { kind: 'back' });
  assert.deepEqual(run([':']), { kind: 'cmdline' });
  assert.deepEqual(run([{ key: 'h', ctrlKey: true }]), { kind: 'focus-tree' });
  assert.deepEqual(run(['i']), { kind: 'edit', how: 'i', replay: false });
  for (const k of ['a', 'A', 'I', 'o', 'O']) assert.deepEqual(run([k]), { kind: 'edit', how: k, replay: false }, k);
  assert.deepEqual(run([{ key: 's', ctrlKey: true }]), { kind: 'save' });
  assert.deepEqual(run([{ key: 'k', ctrlKey: true }]), { kind: 'pass' }, 'the palette chord passes');
  assert.deepEqual(run([{ key: 'k', metaKey: true }]), { kind: 'pass' });
  assert.deepEqual(run(['x']), { kind: 'edit', how: 'normal', replay: true }, 'a normal-mode edit key starts the editor and runs there');
  assert.deepEqual(run(['Z']), { kind: 'swallow' }, 'unbound plain keys never reach the session list');
});

test('the viewer : line quits, jumps, reloads and saves', () => {
  assert.deepEqual(viewerCommand('q'), { kind: 'quit', force: false });
  assert.deepEqual(viewerCommand(':q!'), { kind: 'quit', force: true });
  assert.deepEqual(viewerCommand('42'), { kind: 'goto', line: 42 });
  assert.deepEqual(viewerCommand('e!'), { kind: 'reload', force: true });
  assert.deepEqual(viewerCommand('w'), { kind: 'save', force: false, close: false });
  assert.deepEqual(viewerCommand('w!'), { kind: 'save', force: true, close: false });
  assert.deepEqual(viewerCommand('wq'), { kind: 'save', force: false, close: true });
  assert.deepEqual(viewerCommand('set autopair'), { kind: 'autopair', on: true });
  assert.deepEqual(viewerCommand('set noautopair'), { kind: 'autopair', on: false });
  assert.deepEqual(viewerCommand('  '), { kind: 'none' });
});

test('read states: lines, binary and truncated', () => {
  assert.deepEqual(splitLines('a\nb\n'), ['a', 'b']);
  assert.deepEqual(splitLines('a\r\nb'), ['a', 'b']);
  assert.deepEqual(splitLines(''), []);
  assert.deepEqual(splitLines('\n'), ['']);
  assert.equal(readSummary({ size: 2048, binary: true, truncated: false }, 0, 1 << 20), 'Binary file, 2.0 KiB: not shown');
  assert.equal(readSummary({ size: 3 << 20, binary: false, truncated: true }, 1200, 1 << 20), '1200 lines, 3.0 MiB; truncated at 1.0 MiB');
  assert.equal(readSummary({ size: 5, binary: false, truncated: false }, 1, 1 << 20), '1 line, 5 B');
  assert.equal(canReadMore({ truncated: true, binary: false }, 1 << 20), true);
  assert.equal(canReadMore({ truncated: true, binary: false }, 4 << 20), false);
  assert.equal(canReadMore({ truncated: false, binary: false }, 1 << 20), false);
});

// ---------- explorer keys ----------

test('explorer keys follow the TUI table; a A r m c d u open the file operations', () => {
  const k = (key, more = {}, pending = '') => treeKey(pending, { key, ...more }).cmd;
  assert.deepEqual(k('j'), { kind: 'move', n: 1 });
  assert.deepEqual(k('ArrowUp'), { kind: 'move', n: -1 });
  assert.deepEqual(k('g'), { kind: 'move', n: -Infinity });
  assert.deepEqual(k('End'), { kind: 'move', n: Infinity });
  assert.deepEqual(k('d', { ctrlKey: true }), { kind: 'half', n: 1 });
  assert.deepEqual(k('PageUp'), { kind: 'half', n: -2 });
  assert.deepEqual(k('Enter'), { kind: 'activate' });
  assert.deepEqual(k('l'), { kind: 'open-node' });
  assert.deepEqual(k('h'), { kind: 'close-node' });
  assert.deepEqual(k('Tab'), { kind: 'preview' });
  assert.deepEqual(k('P'), { kind: 'follow' });
  assert.deepEqual(k('z'), { kind: 'collapse-all' });
  assert.deepEqual(k('R'), { kind: 'refresh' });
  assert.deepEqual(k('/'), { kind: 'finder' });
  assert.deepEqual(k('.'), { kind: 'hidden' });
  assert.deepEqual(k('H'), { kind: 'hidden' });
  assert.deepEqual(k('i'), { kind: 'info' });
  assert.deepEqual(k(' '), { kind: 'close' });
  assert.deepEqual(k('q'), { kind: 'close' });
  assert.deepEqual(k('Escape'), { kind: 'close' });
  assert.deepEqual(k('l', { ctrlKey: true }), { kind: 'focus-viewer' });
  assert.deepEqual(k('ArrowRight', { ctrlKey: true, shiftKey: true }), { kind: 'resize', by: 1 });
  assert.deepEqual(k('='), { kind: 'reset-width' });
  for (const [key, op] of [['a', 'add'], ['A', 'mkdir'], ['r', 'rename'], ['m', 'move'], ['c', 'copy'], ['d', 'delete'], ['u', 'undo']]) {
    assert.deepEqual(k(key), { kind: 'op', op }, key);
  }
  assert.deepEqual(treeKey('', { key: 'y' }), { cmd: { kind: 'swallow' }, pending: 'y' });
  assert.deepEqual(k('y', {}, 'y'), { kind: 'yank', what: 'absolute' });
  assert.deepEqual(k('n', {}, 'y'), { kind: 'yank', what: 'name' });
  assert.deepEqual(k('r', {}, 'y'), { kind: 'yank', what: 'relative' });
  assert.deepEqual(k('j', {}, 'y'), { kind: 'move', n: 1 }, 'another key cancels the chord and acts');
  assert.deepEqual(k('k', { ctrlKey: true }), { kind: 'pass' });
  assert.equal(drawerWidth(300, 1, 1000), 324);
  assert.equal(drawerWidth(300, -10, 1000), 180);
  assert.equal(drawerWidth(590, 1, 1000), 600);
});

// ---------- roots, refusals, info ----------

test('the root is the open session’s project folder, else its working dir, else the filtered project', () => {
  const projects = [{ id: P, name: 'rsi', path: '/Users/me/rsi' }, { id: 'p2', name: 'nopath', path: null }];
  assert.deepEqual(chooseRoot({ id: S, project_id: P, working_dir: '/tmp/sb' }, projects, null),
    { ok: true, root: { project_id: P }, key: `p:${P}`, label: 'rsi' });
  assert.deepEqual(chooseRoot({ id: S, project_id: 'p2', working_dir: '/tmp/sandbox/' }, projects, null),
    { ok: true, root: { session_id: S }, key: `s:${S}`, label: 'sandbox' });
  assert.deepEqual(chooseRoot(undefined, projects, P), { ok: true, root: { project_id: P }, key: `p:${P}`, label: 'rsi' });
  assert.deepEqual(chooseRoot(undefined, projects, 'p2'), { ok: false, why: NO_ROOT });
  assert.deepEqual(chooseRoot(undefined, projects, null), { ok: false, why: NO_ROOT });
});

test('refusals read as calm sentences named by their code', () => {
  const rpc = (code, detail) => new Error(`daemon error -32602: Invalid params: ${code}: ${detail}`);
  assert.equal(fileErrorText(rpc('project_file_outside_root', "'../x' has a '..' component")),
    'That path is outside the project folder, so rsid will not open it.');
  assert.equal(fileErrorText(rpc('project_file_not_found', 'gone')), 'That file or folder is no longer there. Refresh the tree (R).');
  assert.equal(fileErrorText(rpc('project_file_not_a_file', 'a dir')), 'That is not a regular file (a folder, a device or a link to one).');
  assert.match(fileErrorText(rpc('project_file_unknown_root', 'x')), /no folder for this project/);
  assert.equal(fileErrorText(rpc('project_file_invalid_request', 'bad range')), 'rsid refused the request. bad range');
  assert.match(fileErrorText(new Error('daemon error -32601: Method not found')), /does not serve project files yet/);
  assert.equal(fileErrorText(new Error('cannot reach rsid: /x/daemon.sock: refused')), 'rsid is not running, so files cannot be read.');
  assert.equal(fileErrorText('daemon error -32000: boom'), 'Could not read files: boom');
});

test('info and absolute paths', () => {
  assert.equal(entryInfo(entry('src/a.rs', 'file', { size: 2048, mtime: '2026-10-18T10:00:00.123456789Z', ignored: true })),
    'src/a.rs: 2.0 KiB, modified 2026-10-18 10:00:00 UTC (ignored by git)');
  assert.equal(entryInfo(entry('.github', 'dir')), '.github: folder (hidden)');
  assert.equal(absolutePath('/private/tmp/p', 'src/a.rs'), '/private/tmp/p/src/a.rs');
  assert.equal(absolutePath('/private/tmp/p/', ''), '/private/tmp/p/');
  assert.equal(absolutePath('\\\\?\\C:\\work\\p', 'src/a.rs'), '\\\\?\\C:\\work\\p\\src\\a.rs');
});

// ---------- registry ----------

const snap = (files, over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, files, ...over,
});
const F = (over = {}) => ({ hasRoot: true, explorer: false, viewer: false, picker: false, showHidden: false, followPreview: false, target: false, editing: false, dirty: false, selection: false, ...over });

test('the slice registers <Space>e and <Space><Space> without conflicts', () => {
  assert.deepEqual(sliceProblems(), []);
  assert.equal(SLICE.order, 55);
  for (const a of FILES_ACTIONS) {
    assert.ok(ACTIONS.some((x) => x.id === a.id), a.id);
    assert.equal(typeof FILES_RULES[a.id], 'function', a.id);
    assert.ok(isFilesAction(a.id));
  }
  assert.deepEqual(Object.keys(FILES_KEY_BINDINGS), ['<Space>e', '<Space><Space>']);
  assert.deepEqual(keyHints('files-explorer'), ['<Space>e']);
  assert.deepEqual(keyHints('files-find'), ['<Space><Space>']);
  const engine = new KeyEngine(desktopKeymap());
  assert.equal(engine.feed('Space').kind, 'pending');
  assert.deepEqual(engine.feed('e'), { kind: 'run', action: 'files-explorer', count: null, keys: '<Space>e' });
  assert.equal(engine.feed('Space').kind, 'pending');
  assert.deepEqual(engine.feed('Space'), { kind: 'run', action: 'files-find', count: null, keys: '<Space><Space>' });
});

test('availability: a root is needed to open, the drawer for its own actions', () => {
  assert.equal(unavailable('files-explorer', snap(F())), null);
  assert.equal(unavailable('files-explorer', snap(F({ hasRoot: false, noRoot: NO_ROOT }))), NO_ROOT);
  assert.equal(unavailable('files-explorer', snap(F({ hasRoot: false, explorer: true }))), null, 'closing never needs a root');
  assert.equal(unavailable('files-find', snap(F({ hasRoot: false, noRoot: NO_ROOT }))), NO_ROOT);
  assert.match(unavailable('files-refresh', snap(F())), /file explorer first/);
  assert.equal(unavailable('files-refresh', snap(F({ explorer: true }))), null);
  assert.match(unavailable('files-close-viewer', snap(F())), /No file is open/);
  assert.equal(unavailable('files-close-viewer', snap(F({ viewer: true }))), null);
  assert.match(unavailable('files-copy-path', snap(F())), /Select a file/);
  assert.equal(unavailable('files-copy-path', snap(F({ viewer: true, target: true }))), null);
  assert.equal(unavailable('files-info', snap(F({ explorer: true, target: true }))), null);
});

// ---------- request shapes ----------

test('the webview sends one request object per command, with exactly one root', async () => {
  const calls = [];
  globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => { calls.push({ cmd, args }); return { ok: true }; } } };
  try {
    const { filesApi } = await import('../dist/files-api.js');
    await filesApi.listDirectory({ project_id: P }, 'src', 2000);
    await filesApi.listFiles({ session_id: S }, 20000);
    await filesApi.readFile({ project_id: P }, 'src/a.rs', 1 << 20);
    await filesApi.gitGutter({ session_id: S }, 'src/a.rs');
    await filesApi.listDirectory({ project_id: P }, '');
  } finally {
    delete globalThis.__TAURI__;
  }
  assert.deepEqual(calls, [
    { cmd: 'list_project_directory', args: { request: { project_id: P, path: 'src', max_entries: 2000 } } },
    { cmd: 'list_project_files', args: { request: { session_id: S, path: '', max_paths: 20000 } } },
    { cmd: 'read_project_file', args: { request: { project_id: P, path: 'src/a.rs', max_bytes: 1048576 } } },
    { cmd: 'get_project_file_git_gutter', args: { request: { session_id: S, path: 'src/a.rs' } } },
    { cmd: 'list_project_directory', args: { request: { project_id: P, path: '' } } },
  ]);
});
