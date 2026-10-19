// File editing and the explorer's changes (#113, hub #3012): the buffer rules,
// the save fence decisions, the add / rename / move / copy / delete / undo
// prompts, the TUI's messages and refusals, and the request shapes the webview
// sends for them. Pure, over a synthetic project; the real-rsid run is
// scripts/smoke.mjs --isolated-write (docs in README "Project files").
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  DraftStore, MAX_WRITE_BYTES, UNSAVED, UNSAVED_RELOAD, editRefusal, eolOf, failedText, isDirty, lineCol, lineOffset, normalEntryOffset,
  refusedChangedText, refusedUnverifiedText, saveRequest, savedText, staleVerdict, toBuffer, toDisk, tooLargeToSave, upToDateText,
} from '../dist/files-edit-model.js';
import {
  completeInput, confirmKey, createdText, intoDirectory, isConfirmation, opMenu, planOp, promptLabel, resolveDestination, splitInput,
  startOp, stemCursor, touched, transferredText, trashedText, undoText,
} from '../dist/files-ops-model.js';
import { refusalCode, refusalDetail, writeErrorText } from '../dist/files-model.js';
import { ACTIONS, unavailable } from '../dist/registry.js';
import { FILES_ACTIONS, FILES_RULES } from '../dist/actions/files.js';

const P = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';
const rpc = (code, detail = 'x') => new Error(`daemon error -32602: Invalid params: ${code}: ${detail}`);

// ---------- the buffer ----------

test('line endings: LF and CRLF files round-trip, a mixed one keeps each line\'s own break', () => {
  assert.equal(eolOf('a\nb\n'), 'lf');
  assert.equal(eolOf('no newline'), 'lf');
  assert.equal(eolOf(''), 'lf');
  assert.equal(eolOf('a\r\nb\r\n'), 'crlf');
  assert.equal(eolOf('a\r\nb\n').kind, 'mixed');
  assert.equal(eolOf('a\rb').kind, 'mixed', 'a lone CR');
  assert.equal(toBuffer('a\r\nb\r\n', 'crlf'), 'a\nb\n');
  assert.equal(toDisk('a\nb\n', 'crlf'), 'a\r\nb\r\n');
  assert.equal(toDisk('a\nb\n', 'lf'), 'a\nb\n');
  assert.equal(toDisk(toBuffer('x\r\ny', 'crlf'), 'crlf'), 'x\r\ny');
});

test('mixed line endings: every unchanged line keeps its own break on save (#134)', () => {
  const disk = 'one\r\ntwo\nthree\r\nfour\rfive';
  const eol = eolOf(disk);
  assert.equal(toBuffer(disk, eol), 'one\ntwo\nthree\nfour\nfive');
  assert.equal(toDisk(toBuffer(disk, eol), eol), disk, 'untouched text is written back byte for byte');
  // Editing inside a line changes no break.
  assert.equal(toDisk('one\ntwo!\nthree\nfour\nfive', eol), 'one\r\ntwo!\nthree\r\nfour\rfive');
  // A line added after a CRLF line gets the line above's break; the lines below keep theirs.
  assert.equal(toDisk('one\nNEW\ntwo\nthree\nfour\nfive', eol), 'one\r\nNEW\r\ntwo\nthree\r\nfour\rfive');
  // A deleted line takes its own break with it.
  assert.equal(toDisk('one\nthree\nfour\nfive', eol), 'one\r\nthree\r\nfour\rfive');
  // Text appended at the end after the last break.
  assert.equal(toDisk('one\ntwo\nthree\nfour\nfive\nsix', eol), 'one\r\ntwo\nthree\r\nfour\rfive\rsix');
  // A mixed file with a trailing newline and an emptied buffer.
  const t = eolOf('a\r\nb\n');
  assert.equal(toDisk('a\nb\n', t), 'a\r\nb\n');
  assert.equal(toDisk('', t), '');
  // The save request carries it.
  assert.equal(saveRequest({ project_id: P }, 'a', 'a\nb!\n', t, 'v', false).content, 'a\r\nb!\n');
  assert.equal(tooLargeToSave('a\nb', eolOf('a\r\nb\r\nc\n')), false);
  assert.deepEqual(staleVerdict('a\nb', t, read2('a\r\nb'), null), { kind: 'up-to-date', version: 'v1:9:9:new' });
});

const read2 = (content) => ({ binary: false, truncated: false, content, version: 'v1:9:9:new' });

test('a file opens for editing only when it was read whole, with a version and one line-ending style', () => {
  const ok = { binary: false, truncated: false, content: 'a\n', version: 'v1:2:1:ab' };
  assert.equal(editRefusal(ok, false), null);
  assert.match(editRefusal({ ...ok, binary: true }, false), /binary/i);
  assert.match(editRefusal({ ...ok, truncated: true }, true), /Read up to 4 MiB/);
  assert.match(editRefusal({ ...ok, truncated: true }, false), /too large/);
  assert.match(editRefusal({ ...ok, version: undefined }, false), /fingerprint/);
  assert.equal(editRefusal({ ...ok, content: 'a\r\nb\n' }, false), null, 'mixed line endings are edited, not refused');
  assert.match(editRefusal({ binary: false, truncated: false, version: 'v' }, false), /not loaded/);
});

test('the save request carries the observed version, the buffer in its own line endings, and force only when asked', () => {
  assert.deepEqual(saveRequest({ project_id: P }, 'src/a.rs', 'x\ny\n', 'lf', 'v1:4:7:ab', false), {
    project_id: P, path: 'src/a.rs', content: 'x\ny\n', expected_version: 'v1:4:7:ab',
  });
  assert.deepEqual(saveRequest({ session_id: P }, 'a', 'x\ny', 'crlf', 'v1:4:7:ab', true), {
    session_id: P, path: 'a', content: 'x\r\ny', expected_version: 'v1:4:7:ab', force: true,
  });
  assert.deepEqual(saveRequest({ project_id: P }, 'a', '', 'lf', null, true), { project_id: P, path: 'a', content: '', force: true });
  assert.equal(tooLargeToSave('x'.repeat(MAX_WRITE_BYTES), 'lf'), false);
  assert.equal(tooLargeToSave('x'.repeat(MAX_WRITE_BYTES + 1), 'lf'), true);
  assert.equal(tooLargeToSave('é'.repeat(MAX_WRITE_BYTES / 2 + 1), 'lf'), true, 'bytes, not characters');
  assert.equal(tooLargeToSave('\n'.repeat(MAX_WRITE_BYTES / 2 + 1), 'crlf'), true, 'CRLF doubles the breaks');
});

test('a stale save is decided from a fresh read, as the TUI does', () => {
  const read = (content, over = {}) => ({ binary: false, truncated: false, content, version: 'v1:9:9:new', ...over });
  // The buffer already equals the disk: nothing is lost, nothing is written.
  assert.deepEqual(staleVerdict('same\n', 'lf', read('same\n'), null), { kind: 'up-to-date', version: 'v1:9:9:new' });
  assert.deepEqual(staleVerdict('a\nb', 'crlf', read('a\r\nb'), null), { kind: 'up-to-date', version: 'v1:9:9:new' });
  // The disk differs from the buffer: refused.
  assert.deepEqual(staleVerdict('mine\n', 'lf', read('theirs\n'), null), { kind: 'refused' });
  // The file is gone: a user-initiated save creates it again.
  assert.deepEqual(staleVerdict('mine\n', 'lf', null, 'project_file_not_found'), { kind: 'recreate' });
  // Anything that cannot be compared is refused as unverifiable.
  assert.deepEqual(staleVerdict('mine\n', 'lf', null, 'project_file_io_failed'), { kind: 'unverified' });
  assert.deepEqual(staleVerdict('mine\n', 'lf', read('', { binary: true, content: undefined }), null), { kind: 'unverified' });
  assert.deepEqual(staleVerdict('mine\n', 'lf', read('mi', { truncated: true }), null), { kind: 'unverified' });
  assert.deepEqual(staleVerdict('x', 'lf', read('x', { version: undefined }), null), { kind: 'refused' }, 'equal text without a fresh version cannot move the fence');
});

test('the save messages are the TUI’s', () => {
  assert.equal(savedText('src/a.rs', false, false), 'Saved: src/a.rs');
  assert.equal(savedText('src/a.rs', true, false), 'Saved: src/a.rs (overwrote the disk copy)');
  assert.equal(savedText('new.rs', true, true), 'Saved: new.rs (created)');
  assert.equal(upToDateText('a'), 'Already up to date: a');
  assert.equal(refusedChangedText('a.rs'), 'Save refused — file changed on disk: a.rs (use :w! to overwrite or :e! to reload)');
  assert.equal(refusedUnverifiedText('a.rs'), 'Save refused — cannot verify file state: a.rs (use :w! to overwrite or :e! to reload)');
  assert.equal(failedText('a.rs', 'disk full'), 'Save failed: a.rs: disk full');
  assert.equal(UNSAVED, 'No write since last change (use :q! to override)');
  assert.match(UNSAVED_RELOAD, /:e!/);
});

test('drafts are kept per root and path, follow a move and mark the unsaved files', () => {
  const d = (path, text, base = 'base') => ({ path, text, base, version: 'v', eol: 'lf', conflict: false, cursor: 0 });
  const store = new DraftStore();
  store.put('p:1', d('src/a.rs', 'edited'));
  store.put('p:1', d('src/b.rs', 'base'));
  store.put('p:2', d('src/a.rs', 'other root'));
  assert.deepEqual([...store.dirty('p:1')], ['src/a.rs'], 'only a buffer that differs from the disk is unsaved');
  assert.equal(store.anyDirty(), true);
  store.retarget('p:1', 'src', 'lib');
  assert.equal(store.get('p:1', 'src/a.rs'), undefined);
  assert.equal(store.get('p:1', 'lib/a.rs').text, 'edited');
  assert.equal(store.get('p:2', 'src/a.rs').text, 'other root', 'another root is left alone');
  store.retarget('p:1', 'lib/a.rs', 'lib/c.rs');
  assert.deepEqual([...store.dirty('p:1')], ['lib/c.rs']);
  store.drop('p:1', 'lib/c.rs');
  store.drop('p:2', 'src/a.rs');
  assert.equal(store.anyDirty(), false);
  assert.equal(isDirty({ text: 'a', base: 'a' }), false);
});

test('the caret helpers: normal-mode entry on the cursor character, :<n> and the status position', () => {
  const t = 'one\ntwo\nthree';
  // Leaving insert steps one column left, so a start in the middle of a line is handed one on.
  assert.equal(normalEntryOffset(t, 0), 0);
  assert.equal(normalEntryOffset(t, 5), 6, 'column 1 of line two');
  assert.equal(normalEntryOffset(t, 4), 4, 'column 0 needs no step');
  assert.equal(normalEntryOffset(t, 3), 3, 'on a line break');
  assert.equal(normalEntryOffset(t, t.length), t.length);
  assert.equal(lineOffset(t, 1), 0);
  assert.equal(lineOffset(t, 2), 4);
  assert.equal(lineOffset(t, 3), 8);
  assert.equal(lineOffset(t, 99), 8, 'past the end lands on the last line');
  assert.deepEqual(lineCol(t, 0), { line: 0, col: 0 });
  assert.deepEqual(lineCol(t, 6), { line: 1, col: 2 });
  assert.deepEqual(lineCol(t, 13), { line: 2, col: 5 });
});

// ---------- the explorer prompts ----------

const file = { path: 'src/main.rs', name: 'main.rs', isDir: false };
const dir = { path: 'src', name: 'src', isDir: true };

test('prompts open as the TUI’s do: add in the selected folder, rename on the name, move and copy on the path', () => {
  assert.equal(startOp('rename', null, ''), 'Nothing selected');
  assert.equal(startOp('delete', null, ''), 'Nothing selected');
  assert.deepEqual(startOp('add', null, ''), { op: 'add', target: null, dir: '', input: '', cursor: 0 });
  const rename = startOp('rename', file, 'src');
  assert.equal(rename.input, 'main.rs');
  assert.equal(rename.cursor, 4, 'the caret sits before the extension');
  assert.equal(startOp('rename', dir, '').cursor, 3, 'a folder name is not split at a dot');
  assert.equal(stemCursor('.gitignore'), null);
  assert.equal(stemCursor('a.tar.gz'), 5);
  const mv = startOp('move', file, 'src');
  assert.equal(mv.input, 'src/main.rs');
  assert.equal(startOp('copy', file, 'src').input, 'src/main.rs');
  assert.equal(promptLabel(startOp('add', null, '')), 'New file in ./ (end with / for dir)');
  assert.equal(promptLabel(startOp('add', file, 'src')), 'New file in src/ (end with / for dir)');
  assert.equal(promptLabel(startOp('mkdir', null, 'src/ui')), 'New directory in src/ui/');
  assert.equal(promptLabel(rename), 'Rename main.rs');
  assert.equal(promptLabel(mv), 'Move main.rs to');
  assert.equal(promptLabel(startOp('copy', file, '')), 'Copy main.rs to');
  assert.equal(promptLabel(startOp('delete', file, '')), 'Delete main.rs?');
  assert.equal(promptLabel(startOp('delete', dir, '')), 'Delete src/ and its contents?');
  assert.equal(isConfirmation(startOp('delete', file, '')), true);
  assert.equal(isConfirmation(rename), false);
});

test('delete asks: Enter, y, Y and d confirm; Esc, n, N and q cancel; nothing else decides', () => {
  for (const k of ['Enter', 'y', 'Y', 'd']) assert.equal(confirmKey(k, false), 'yes', k);
  for (const k of ['Escape', 'n', 'N', 'q']) assert.equal(confirmKey(k, false), 'no', k);
  assert.equal(confirmKey('d', true), null, 'Ctrl-d is not a yes');
  assert.equal(confirmKey('x', false), null);
});

test('a destination stays inside the root: relative, absolute inside, never above or at the root', () => {
  const r = (base, input, abs = '/work/proj') => resolveDestination('proj', abs, base, input);
  assert.deepEqual(r('src', 'new.rs'), { ok: true, path: 'src/new.rs' });
  assert.deepEqual(r('src', '../lib/x.rs'), { ok: true, path: 'lib/x.rs' });
  assert.deepEqual(r('', 'a/./b//c/'), { ok: true, path: 'a/b/c' }, 'trailing slash, dots and double slashes are tidied');
  assert.deepEqual(r('src', '/work/proj/docs/a.md'), { ok: true, path: 'docs/a.md' }, 'an absolute path inside the root');
  assert.deepEqual(r('src', '  spaced.txt  '), { ok: true, path: 'src/spaced.txt' });
  const outside = { ok: false, error: 'Path must stay inside proj' };
  assert.deepEqual(r('src', '../../x'), outside);
  assert.deepEqual(r('', '..'), outside);
  assert.deepEqual(r('src', '..'), outside, 'the root itself');
  assert.deepEqual(r('', '/etc/passwd'), outside);
  assert.deepEqual(r('', '/work/proj-other/x'), outside, 'a sibling that shares the prefix');
  assert.deepEqual(r('', '/work/proj'), outside);
  assert.deepEqual(r('', '/work/proj/x', null), outside, 'an absolute path with no known root is not guessed');
  assert.deepEqual(r('', '   '), { ok: false, error: 'Name cannot be empty' });
  assert.deepEqual(r('', '/'), { ok: false, error: 'Name cannot be empty' });
  assert.deepEqual(r('', 'a\u0000b'), { ok: false, error: 'Name cannot contain a NUL byte' });
});

test('submitting a prompt becomes one rsid request: create, rename, move or copy', () => {
  const plan = (op, target, dirIn, input) => planOp({ ...startOp(op, target, dirIn), input }, 'proj', '/work/proj');
  assert.deepEqual(plan('add', null, 'src', 'a.rs'), { kind: 'create', path: 'src/a.rs', as: 'file' });
  assert.deepEqual(plan('add', null, 'src', 'ui/'), { kind: 'create', path: 'src/ui', as: 'dir' }, 'a trailing / adds a folder');
  assert.deepEqual(plan('add', null, '', 'deep/er/x.txt'), { kind: 'create', path: 'deep/er/x.txt', as: 'file' });
  assert.deepEqual(plan('mkdir', null, '', 'docs'), { kind: 'create', path: 'docs', as: 'dir' });
  assert.deepEqual(plan('add', null, '', '../x'), { kind: 'error', error: 'Path must stay inside proj' });
  assert.deepEqual(plan('add', null, '', ''), { kind: 'error', error: 'Name cannot be empty' });
  assert.deepEqual(plan('rename', file, 'src', 'lib.rs'), { kind: 'rename', from: 'src/main.rs', to: 'src/lib.rs' });
  assert.deepEqual(plan('rename', file, 'src', 'main.rs'), { kind: 'noop' }, 'an unchanged name does nothing');
  assert.deepEqual(plan('rename', file, 'src', '../top.rs'), { kind: 'rename', from: 'src/main.rs', to: 'top.rs' });
  assert.deepEqual(plan('move', file, 'src', 'lib/main.rs'), { kind: 'move', from: 'src/main.rs', to: 'lib/main.rs' });
  assert.deepEqual(plan('copy', file, 'src', 'src/main.rs.bak'), { kind: 'copy', from: 'src/main.rs', to: 'src/main.rs.bak' });
  assert.deepEqual(plan('move', file, 'src', '/etc/x'), { kind: 'error', error: 'Path must stay inside proj' });
  assert.deepEqual(plan('delete', file, '', ''), { kind: 'noop' });
});

test('mv a dir/: an existing folder as the destination receives the entry under its own name', () => {
  assert.equal(intoDirectory('src/main.rs', 'lib', true), 'lib/main.rs');
  assert.equal(intoDirectory('src/main.rs', 'lib/new.rs', false), 'lib/new.rs');
  assert.equal(intoDirectory('src', 'src', true), 'src', 'onto itself stays itself (then nothing happens)');
});

test('the messages and the selection after each change are the TUI’s', () => {
  assert.equal(createdText('src/a.rs', 'file'), 'Created src/a.rs');
  assert.equal(createdText('docs', 'dir'), 'Created docs/');
  assert.equal(transferredText('moved', 'src/a.rs', 'src/b.rs'), 'Renamed to b.rs');
  assert.equal(transferredText('moved', 'src/a.rs', 'lib/a.rs'), 'Moved to lib/a.rs');
  assert.equal(transferredText('copied', 'src/a.rs', 'src/a.bak'), 'Copied to src/a.bak');
  assert.equal(trashedText('src/old.rs'), 'Deleted: old.rs (u to undo)');
  assert.deepEqual(undoText({ effect: 'restored', path: 'src/old.rs' }), { message: 'Restored old.rs', select: 'src/old.rs' });
  assert.deepEqual(undoText({ effect: 'moved_back', path: 'src/a.rs' }), { message: 'Moved a.rs back', select: 'src/a.rs' });
  assert.deepEqual(undoText({ effect: 'removed', path: 'src/new.rs' }), { message: 'Removed new.rs', select: 'src' });
  assert.deepEqual(undoText({ effect: 'trashed', path: 'copy.rs' }), { message: 'Moved copy.rs to trash', select: '' });
});

test('the folders to reload and the files to follow after a change', () => {
  const op = (o) => ({ root: '/r', undo_depth: 1, ...o });
  assert.deepEqual(touched(op({ op: 'created', path: 'a/b/c.txt', created_top: 'a/b' })), { dirs: ['a', 'a/b'], moved: null });
  assert.deepEqual(touched(op({ op: 'moved', path: 'src/a.rs', to: 'lib/a.rs' })), { dirs: ['src', 'lib'], moved: { from: 'src/a.rs', to: 'lib/a.rs' } });
  assert.deepEqual(touched(op({ op: 'copied', path: 'src/a.rs', to: 'src/b.rs' })), { dirs: ['src'], moved: null });
  assert.deepEqual(touched(op({ op: 'trashed', path: 'x.txt' })), { dirs: [''], moved: null });
  assert.deepEqual(touched({ root: '/r', undone: 'moved', effect: 'moved_back', path: 'src/a.rs', from: 'lib/a.rs', undo_depth: 0 }),
    { dirs: ['src', 'lib'], moved: { from: 'lib/a.rs', to: 'src/a.rs' } });
  assert.deepEqual(touched({ root: '/r', undone: 'trashed', effect: 'restored', path: 'src/old.rs', undo_depth: 0 }), { dirs: ['src'], moved: null });
});

test('Tab completes the last segment: a unique match fully, several to their common prefix', () => {
  const entries = [
    { name: 'src', isDir: true }, { name: 'scripts', isDir: true }, { name: 'README.md', isDir: false }, { name: '.git', isDir: true }, { name: '.gitignore', isDir: false },
  ];
  assert.equal(completeInput('R', entries, false), 'README.md');
  assert.equal(completeInput('sr', entries, false), 'src/', 'a folder gains a slash');
  assert.equal(completeInput('s', entries, false), null, 'src and scripts share only the s already typed');
  assert.equal(completeInput('', entries, false), null);
  assert.equal(completeInput('.g', entries, false), '.git', 'a dot asks for hidden names');
  assert.equal(completeInput('docs/R', entries, false), 'docs/README.md');
  assert.equal(completeInput('.', [{ name: '.a', isDir: false }, { name: '.ab', isDir: false }], false), '.a');
  assert.equal(completeInput('x', entries, true), null);
  assert.deepEqual(splitInput('a/b/c'), { dirPart: 'a/b/', partial: 'c' });
  assert.deepEqual(splitInput('c'), { dirPart: '', partial: 'c' });
});

test('the right-click menu offers what applies to the entry under the pointer', () => {
  assert.deepEqual(opMenu(null).map((i) => i.op), ['add', 'mkdir', 'undo']);
  assert.deepEqual(opMenu(file).map((i) => i.op), ['add', 'mkdir', 'rename', 'move', 'copy', 'delete', 'undo']);
  assert.equal(opMenu(file).find((i) => i.op === 'delete').key, 'd');
});

// ---------- refusals ----------

test('a refused change reads in the TUI’s words, with rsid’s code deciding', () => {
  assert.equal(refusalCode(rpc('project_file_stale')), 'project_file_stale');
  assert.equal(refusalCode(new Error('boom')), null);
  assert.equal(refusalDetail(rpc('project_file_conflict', 'src/a.rs changed')), 'src/a.rs changed');
  assert.equal(refusalDetail(new Error('boom')), '');
  const ctx = { name: 'a.rs', rootName: 'proj' };
  assert.equal(writeErrorText(rpc('project_file_exists'), ctx), 'a.rs already exists');
  assert.equal(writeErrorText(rpc('project_file_outside_root'), ctx), 'Path must stay inside proj');
  assert.equal(writeErrorText(rpc('project_file_nothing_to_undo')), 'Nothing to undo');
  assert.match(writeErrorText(rpc('project_file_stale')), /changed on disk/);
  assert.match(writeErrorText(rpc('project_file_conflict', 'src/a.rs changed while it was moved')), /Nothing was lost\. src\/a\.rs changed while it was moved/);
  assert.match(writeErrorText(rpc('project_file_undo_conflict', 'src/a.rs exists again')), /^Undo failed: src\/a\.rs exists again/);
  assert.match(writeErrorText(rpc('project_file_undo_conflict', '')), /^Undo failed: /);
  assert.match(writeErrorText(rpc('project_file_too_large')), /8 MiB/);
  assert.match(writeErrorText(rpc('project_file_io_failed', 'Permission denied')), /Permission denied/);
  assert.match(writeErrorText(rpc('project_file_unsupported_platform')), /not supported on this platform/);
  assert.match(writeErrorText(rpc('project_file_not_found')), /no longer there/);
  assert.match(writeErrorText(new Error('daemon error -32601: Method not found')), /does not change project files yet/);
  assert.equal(writeErrorText(new Error('cannot reach rsid: /x: refused')), 'rsid is not running, so files cannot be changed.');
  assert.equal(writeErrorText('daemon error -32000: boom'), 'The change failed: boom');
});

// ---------- registry ----------

const snap = (files) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, files,
});
const F = (over = {}) => ({
  hasRoot: true, explorer: false, viewer: false, picker: false, showHidden: false, followPreview: false, target: false,
  editing: false, dirty: false, selection: false, ...over,
});

test('the edit and explorer-change actions are registered and gated on what they act on', () => {
  for (const id of ['files-edit', 'files-save', 'files-save-force', 'files-reload', 'files-add', 'files-mkdir', 'files-rename', 'files-move', 'files-copy', 'files-delete', 'files-undo']) {
    assert.ok(FILES_ACTIONS.some((a) => a.id === id), id);
    assert.ok(ACTIONS.some((a) => a.id === id), `${id} in the registry`);
    assert.equal(typeof FILES_RULES[id], 'function', id);
  }
  assert.match(unavailable('files-edit', snap(F())), /No file is open/);
  assert.equal(unavailable('files-edit', snap(F({ viewer: true }))), null);
  assert.match(unavailable('files-edit', snap(F({ viewer: true, editing: true }))), /already being edited/);
  assert.match(unavailable('files-save', snap(F({ viewer: true }))), /not being edited/);
  assert.equal(unavailable('files-save', snap(F({ viewer: true, editing: true, dirty: true }))), null);
  assert.equal(unavailable('files-save-force', snap(F({ viewer: true, editing: true }))), null);
  assert.match(unavailable('files-reload', snap(F())), /No file is open/);
  assert.match(unavailable('files-add', snap(F())), /file explorer first/);
  assert.equal(unavailable('files-add', snap(F({ explorer: true }))), null);
  assert.equal(unavailable('files-undo', snap(F({ explorer: true }))), null);
  assert.match(unavailable('files-rename', snap(F({ explorer: true }))), /Select a file or folder/);
  assert.match(unavailable('files-delete', snap(F())), /file explorer first/);
  for (const id of ['files-rename', 'files-move', 'files-copy', 'files-delete']) {
    assert.equal(unavailable(id, snap(F({ explorer: true, selection: true }))), null, id);
  }
});

// ---------- request shapes ----------

test('the webview sends one request object per change, with exactly one root', async () => {
  const calls = [];
  globalThis.__TAURI__ = { core: { invoke: async (cmd, args) => { calls.push({ cmd, args }); return { ok: true }; } } };
  try {
    const { filesApi } = await import('../dist/files-api.js');
    await filesApi.writeFile(saveRequest({ project_id: P }, 'src/a.rs', 'x\n', 'lf', 'v1:2:1:ab', false));
    await filesApi.createEntry({ project_id: P }, 'docs/', 'dir');
    await filesApi.moveEntry({ session_id: P }, 'a.rs', 'src/a.rs');
    await filesApi.copyEntry({ session_id: P }, 'a.rs', 'b.rs');
    await filesApi.trashEntry({ project_id: P }, 'old.txt');
    await filesApi.undo({ project_id: P });
  } finally {
    delete globalThis.__TAURI__;
  }
  assert.deepEqual(calls, [
    { cmd: 'write_project_file', args: { request: { project_id: P, path: 'src/a.rs', content: 'x\n', expected_version: 'v1:2:1:ab' } } },
    { cmd: 'create_project_entry', args: { request: { project_id: P, path: 'docs/', kind: 'dir' } } },
    { cmd: 'move_project_entry', args: { request: { session_id: P, from: 'a.rs', to: 'src/a.rs' } } },
    { cmd: 'copy_project_entry', args: { request: { session_id: P, from: 'a.rs', to: 'b.rs' } } },
    { cmd: 'trash_project_entry', args: { request: { project_id: P, path: 'old.txt' } } },
    { cmd: 'undo_project_file_operation', args: { request: { project_id: P } } },
  ]);
});
