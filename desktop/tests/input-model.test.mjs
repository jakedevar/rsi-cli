import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  Jumplist, NO_GEOMETRY, acceptSlash, columnStep, compileFailure, compileOutcome, configGate, filterSlash, fuzzyScore,
  dataUrlBase64, geometryStep, imageExtension, imageReference, messagePreview, pasteImageMime, slashRoot, slashRootKey, moveSuggestion, operatorSlash, paneStep, parseGeometry,
  parseOperatorCommand, pendingMessages, previewKey, queuesFollowUp, recentFiles, relativeTo, routeGlobal, sidebarStep,
  slashQuery, stepRow, submitKey, toggleSequence,
} from '../dist/input-model.js';
import { KeyEngine, SEQUENCE_TIMEOUT_MS, buildKeymap } from '../dist/keyengine.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { unavailable } from '../dist/registry.js';
import { INPUT_ACTIONS, INPUT_KEY_BINDINGS, INPUT_PREFIX_COMMANDS, INPUT_RULES } from '../dist/actions/input-spec.js';

const env = (o = {}) => ({
  overlay: false, typing: false, composer: 'none', focus: 'list', sessionOpen: true, physicalCtrl: true, ...o,
});

// ---------- global intercepts ----------

test('overlay geometry keys need an open overlay and the physical Ctrl key', () => {
  assert.deepEqual(routeGlobal('C-Up', env({ overlay: true })), { kind: 'geometry', op: 'up' });
  assert.deepEqual(routeGlobal('C-S-Right', env({ overlay: true })), { kind: 'geometry', op: 'wider' });
  assert.deepEqual(routeGlobal('C-S-Up', env({ overlay: true })), { kind: 'geometry', op: 'shorter' });
  assert.deepEqual(routeGlobal('C-0', env({ overlay: true, typing: true })), { kind: 'geometry', op: 'reset' });
  assert.equal(routeGlobal('C-Up', env({ overlay: true, physicalCtrl: false })), null);
  assert.equal(routeGlobal('C-o', env({ overlay: true })), null);
  assert.equal(routeGlobal('Left', env({ overlay: true })), null);
});

test('jumplist and pane keys work unless something is being typed', () => {
  assert.deepEqual(routeGlobal('C-o', env()), { kind: 'jump', dir: -1 });
  assert.deepEqual(routeGlobal('C-i', env({ composer: 'normal' })), { kind: 'jump', dir: 1 });
  assert.deepEqual(routeGlobal('C-i', env({ composer: 'standard' })), { kind: 'jump', dir: 1 });
  assert.equal(routeGlobal('C-o', env({ composer: 'insert' })), null);
  assert.equal(routeGlobal('C-o', env({ typing: true })), null);
});

test('Ctrl-H/L switch zones in the session list and move pane focus elsewhere', () => {
  assert.deepEqual(routeGlobal('C-l', env({ focus: 'list' })), { kind: 'zone', dir: 1 });
  assert.deepEqual(routeGlobal('C-h', env({ focus: 'list' })), { kind: 'zone', dir: -1 });
  assert.deepEqual(routeGlobal('C-h', env({ focus: 'detail' })), { kind: 'pane', dir: -1 });
  assert.deepEqual(routeGlobal('C-h', env({ composer: 'standard', focus: 'detail' })), { kind: 'pane', dir: -1 });
  assert.equal(routeGlobal('C-h', env({ composer: 'insert' })), null);
});

test('Ctrl-Shift-Left/Right size the sidebar; Ctrl-Left/Right move the column or the pane', () => {
  assert.deepEqual(routeGlobal('C-S-Right', env()), { kind: 'sidebar', dir: 1 });
  assert.equal(routeGlobal('C-S-Right', env({ composer: 'standard' })), null);
  assert.deepEqual(routeGlobal('C-Left', env({ focus: 'detail' })), { kind: 'column', dir: -1 });
  assert.deepEqual(routeGlobal('C-Right', env({ focus: 'list' })), { kind: 'pane', dir: 1 });
  assert.deepEqual(routeGlobal('C-Right', env({ focus: 'detail', sessionOpen: false })), { kind: 'pane', dir: 1 });
  assert.equal(routeGlobal('C-Left', env({ composer: 'insert' })), null);
});

test('session-list arrows step sessions; Shift opens; Ctrl-Tab is previous, Ctrl-Shift-Tab next', () => {
  assert.deepEqual(routeGlobal('Left', env()), { kind: 'step', dir: -1, open: false });
  assert.deepEqual(routeGlobal('S-Right', env({ focus: 'detail' })), { kind: 'step', dir: 1, open: true });
  assert.deepEqual(routeGlobal('C-Tab', env()), { kind: 'step', dir: -1, open: false });
  assert.deepEqual(routeGlobal('C-S-Tab', env()), { kind: 'step', dir: 1, open: false });
  assert.equal(routeGlobal('Left', env({ composer: 'normal' })), null);
  assert.equal(routeGlobal('Left', env({ typing: true })), null);
  assert.equal(routeGlobal('x', env()), null);
});

test('pane steps stop at the edges; zones wrap; rows clamp', () => {
  assert.equal(paneStep('detail', -1, true), 'list');
  assert.equal(paneStep('list', -1, true), null);
  assert.equal(paneStep('list', 1, true), 'detail');
  assert.equal(paneStep('list', 1, false), null);
  assert.equal(stepRow(['a', 'b', 'c'], 'b', 1), 'c');
  assert.equal(stepRow(['a', 'b', 'c'], 'c', 1), 'c');
  assert.equal(stepRow(['a', 'b', 'c'], null, -1), 'c');
  assert.equal(stepRow([], null, 1), null);
});

test('the jumplist goes back and forward, drops forward history on a new visit and skips gone sessions', () => {
  const j = new Jumplist();
  for (const id of ['a', 'b', 'c']) j.visit(id);
  const all = () => true;
  assert.equal(j.canJump(1, all), false);
  assert.equal(j.jump(-1, all), 'b');
  assert.equal(j.jump(-1, all), 'a');
  assert.equal(j.jump(-1, all), null);
  assert.equal(j.jump(1, all), 'b');
  j.visit('d');
  assert.deepEqual(j.list(), { entries: ['a', 'b', 'd'], at: 2 });
  assert.equal(j.jump(-1, (id) => id !== 'b'), 'a');
  j.visit('a');
  assert.deepEqual(j.list().entries, ['a', 'b', 'd']);
});

test('overlay geometry moves, resizes, stays inside the window and resets', () => {
  const cur = { width: 400, height: 300 };
  const vp = { width: 1000, height: 800 };
  let g = geometryStep(NO_GEOMETRY, 'right', cur, vp);
  assert.deepEqual(g, { dx: 24, dy: 0, width: null, height: null });
  g = geometryStep(g, 'wider', cur, vp);
  assert.equal(g.width, 424);
  g = geometryStep(g, 'shorter', cur, vp);
  assert.equal(g.height, 276);
  let far = { dx: 5000, dy: -5000, width: null, height: null };
  far = geometryStep(far, 'up', cur, vp);
  assert.deepEqual([far.dx, far.dy], [300, -250]);
  assert.equal(geometryStep(g, 'narrower', { width: 240, height: 120 }, vp).width, 240);
  assert.deepEqual(geometryStep(g, 'reset', cur, vp), NO_GEOMETRY);
  assert.deepEqual(parseGeometry(JSON.stringify(g)), g);
  assert.deepEqual(parseGeometry('nonsense'), NO_GEOMETRY);
});

test('sidebar width and transcript column step within bounds; the column snaps to Left Aligned at 0', () => {
  assert.equal(sidebarStep(340, 1, 1200), 364);
  assert.equal(sidebarStep(210, -1, 1200), 200);
  assert.equal(sidebarStep(830, 1, 1200), 840);
  assert.equal(columnStep(0, 1, 1000), 40);
  assert.equal(columnStep(30, -1, 1000), 0);
  assert.equal(columnStep(660, 1, 1000), 680);
  assert.equal(columnStep(0, 1, 300), 0);
});

// ---------- recent files ----------

test('recent files: newest first, distinct, file tools only, at most nine', () => {
  const ev = (tool_name, file_path) => ({ tool_name, tool_input: { file_path } });
  const events = [
    ev('Read', '/p/a.rs'), ev('Bash', '/p/x'), ev('Edit', '/p/b.rs'), ev('Write', '/p/a.rs'),
    { tool_name: 'MultiEdit', tool_input: null }, ev('MultiEdit', '/p/c.rs'),
  ];
  assert.deepEqual(recentFiles(events), ['/p/c.rs', '/p/a.rs', '/p/b.rs']);
  const many = Array.from({ length: 12 }, (_, i) => ev('Read', `/p/${i}`));
  assert.equal(recentFiles(many).length, 9);
  assert.equal(recentFiles(many)[0], '/p/11');
});

test('relativeTo keeps a path inside its root', () => {
  assert.equal(relativeTo('/p/repo', '/p/repo/src/a.rs'), 'src/a.rs');
  assert.equal(relativeTo('/p/repo/', '/p/repo/a'), 'a');
  assert.equal(relativeTo('/p/repo', '/p/repository/a'), null);
  assert.equal(relativeTo('/p/repo', '/p/repo/../etc'), null);
  assert.equal(relativeTo('C:\\r', 'C:\\r\\x\\y.txt'), 'x/y.txt');
  assert.equal(relativeTo('', '/a'), null);
});

// ---------- config gate, submit, paste ----------

test('the config gate refuses with the TUI notice until the config is ready', () => {
  assert.equal(configGate({ kind: 'ready' }), null);
  assert.equal(configGate({ kind: 'loading' }), 'Daemon configuration is still loading; draft preserved');
  assert.equal(configGate({ kind: 'error', error: 'down' }), 'Daemon configuration unavailable: down; draft preserved');
});

test('Submit on Enter: Enter sends, Shift+Enter is a newline; Ctrl/Cmd+Enter always sends', () => {
  assert.equal(submitKey({ key: 'Enter' }, true), true);
  assert.equal(submitKey({ key: 'Enter', shiftKey: true }, true), false);
  assert.equal(submitKey({ key: 'Enter' }, false), false);
  assert.equal(submitKey({ key: 'Enter', ctrlKey: true }, false), true);
  assert.equal(submitKey({ key: 'Enter', metaKey: true }, false), true);
  assert.equal(submitKey({ key: 'Enter', isComposing: true }, true), false);
  assert.equal(submitKey({ key: 'a' }, true), false);
});

test('pasted images keep the types the TUI saves', () => {
  assert.equal(imageExtension('image/png'), 'png');
  assert.equal(imageExtension('image/JPEG'), 'jpg');
  assert.equal(imageExtension('image/svg+xml'), null);
  assert.equal(imageReference('/h/.rsi/paste/p.png'), '@/h/.rsi/paste/p.png ');
});

test('a pasted image goes to rsid as an allowlisted type and base64 (hub #3044)', () => {
  assert.equal(pasteImageMime('image/png'), 'image/png');
  assert.equal(pasteImageMime('image/jpg'), 'image/jpeg');
  assert.equal(pasteImageMime('IMAGE/WEBP'), 'image/webp');
  assert.equal(pasteImageMime('image/svg+xml'), null);
  assert.equal(dataUrlBase64('data:image/png;base64,iVBORw=='), 'iVBORw==');
  assert.equal(dataUrlBase64('data:image/png;base64,'), null);
  assert.equal(dataUrlBase64('data:text/plain,hello'), null);
});

test('slash suggestions name a project or session root for rsid (hub #3044)', () => {
  const projects = [{ id: 'p1', path: '/repo/' }, { id: 'p2', path: null }];
  assert.deepEqual(slashRoot(null, projects, 's1'), { session_id: 's1' });
  assert.equal(slashRoot(null, projects, null), null);
  assert.deepEqual(slashRoot({ workingDir: '', projectId: 'p2' }, projects, 's1'), { project_id: 'p2' });
  assert.deepEqual(slashRoot({ workingDir: ' /repo ', projectId: 'p2' }, projects, null), { project_id: 'p1' });
  assert.equal(slashRoot({ workingDir: '/elsewhere', projectId: 'p1' }, projects, null), null);
  assert.equal(slashRootKey({ project_id: 'p1' }), 'p:p1');
  assert.equal(slashRootKey({ session_id: 's1' }), 's:s1');
});

// ---------- slash suggestions ----------

test('slash queries start a line and end at the cursor', () => {
  assert.equal(slashQuery('/pla', 4), 'pla');
  assert.equal(slashQuery('/', 1), '');
  assert.equal(slashQuery('hi\n/de', 6), 'de');
  assert.equal(slashQuery('hi /de', 6), null);
  assert.equal(slashQuery('/plan now', 9), null);
  assert.equal(slashQuery('/plan', 0), null);
});

test('fuzzy filter ranks prefixes and runs first; Tab accepts with a space', () => {
  const cmds = [{ name: 'debug', description: '' }, { name: 'plan', description: '' }, { name: 'review_plan', description: '' }];
  assert.deepEqual(filterSlash(cmds, '').map((c) => c.name), ['debug', 'plan', 'review_plan']);
  assert.deepEqual(filterSlash(cmds, 'pl').map((c) => c.name), ['plan', 'review_plan']);
  assert.equal(fuzzyScore('plan', 'xz'), null);
  assert.ok(fuzzyScore('plan', 'pla') > fuzzyScore('review_plan', 'pla'));
  assert.deepEqual(acceptSlash('x\n/pl', 5, 'plan'), { text: 'x\n/plan ', cursor: 8 });
  assert.equal(moveSuggestion(3, 2, 1), 0);
  assert.equal(moveSuggestion(3, 0, -1), 2);
});

// ---------- correction preview ----------

test('compile results become a preview, a CLARIFY or a notice', () => {
  const ok = compileOutcome({ compiled: 'Better', contract: { status: 'complete' }, layer_validation: { semantic: true, syntactic: false } });
  assert.deepEqual(ok.preview, { text: 'Better', clarify: false, kind: 'compile' });
  assert.equal(ok.notice, 'Compiler: weak layers [syntactic]');
  const err = compileOutcome({ compiled: '', contract: { status: 'error', kind: 'refusal', message: 'no' } });
  assert.equal(err.preview, null);
  assert.equal(err.notice, 'Compile ERROR [refusal]: no');
  const clarify = compileFailure('daemon error: Ambiguous intent: which file?');
  assert.deepEqual(clarify.preview, { text: 'CLARIFY:\n- which file?', clarify: true, kind: 'compile' });
  assert.equal(compileFailure('timeout').notice, 'Prompt compilation failed: timeout');
});

test('preview keys: a accepts (not a CLARIFY), d / Esc discard, Ctrl-Enter sends the original, retries', () => {
  const p = { text: 'x', clarify: false, kind: 'compile' };
  assert.equal(previewKey('a', p), 'accept');
  assert.equal(previewKey('a', { ...p, clarify: true }), null);
  assert.equal(previewKey('d', p), 'discard');
  assert.equal(previewKey('Esc', p), 'discard');
  assert.equal(previewKey('C-Enter', p), 'send-original');
  assert.equal(previewKey('C-y', p), 'compile');
  assert.equal(previewKey('C-S-g', p), 'grammar');
  assert.equal(previewKey('j', p), null);
});

// ---------- operator messages ----------

const ID = '0b6f8e1c-4b5a-4f1e-8c2d-3a4b5c6d7e8f';

test(':pending and :drain arguments parse like the TUI', () => {
  assert.deepEqual(parseOperatorCommand('pending', ''), { kind: 'list' });
  assert.deepEqual(parseOperatorCommand('pending', `edit ${ID} new text here`), { kind: 'edit', id: ID, text: 'new text here' });
  assert.deepEqual(parseOperatorCommand('pending', `withdraw ${ID.toUpperCase()}`), { kind: 'withdraw', id: ID });
  assert.equal(parseOperatorCommand('pending', 'edit nope text').kind, 'error');
  assert.equal(parseOperatorCommand('pending', `edit ${ID}`).kind, 'error');
  assert.equal(parseOperatorCommand('pending', 'frob').kind, 'error');
  assert.deepEqual(parseOperatorCommand('drain', ''), { kind: 'drain' });
  assert.deepEqual(parseOperatorCommand('drain', 'status'), { kind: 'drain-status' });
  assert.equal(parseOperatorCommand('drain', 'now').kind, 'error');
});

test('typed /pending and /drain run the command; other text is a message', () => {
  assert.deepEqual(operatorSlash('/pending'), { kind: 'list' });
  assert.deepEqual(operatorSlash(`/pending withdraw ${ID}`), { kind: 'withdraw', id: ID });
  assert.deepEqual(operatorSlash(' /drain status '), { kind: 'drain-status' });
  assert.equal(operatorSlash('/pendingx'), null);
  assert.equal(operatorSlash('please /drain'), null);
});

test('only in-flight messages count as pending; busy sessions queue follow-ups', () => {
  const list = [
    { id: 'a', session_id: 's', content: 'x', state: 'queued' },
    { id: 'b', session_id: 's', content: 'x', state: 'delivered' },
    { id: 'c', session_id: 's', content: 'x', state: 'uncertain' },
    { id: 'd', session_id: 's', content: 'x', state: 'withdrawn' },
  ];
  assert.deepEqual(pendingMessages(list).map((m) => m.id), ['a', 'c']);
  assert.deepEqual(pendingMessages({}), []);
  assert.equal(messagePreview(`a\n  b${'x'.repeat(200)}`).length, 100);
  assert.equal(queuesFollowUp('Running'), true);
  assert.equal(queuesFollowUp('WaitingApproval'), true);
  assert.equal(queuesFollowUp('Completed'), false);
});

// ---------- modal toggles ----------

test('toggle keys: the key that opened a picker closes it', () => {
  assert.deepEqual(toggleSequence(['j', 'T']), ['T']);
  assert.deepEqual(toggleSequence(['Space', 'p']), ['Space', 'p']);
  assert.deepEqual(toggleSequence(['C-m']), ['C-m']);
  assert.equal(toggleSequence(['p']), null);
  assert.equal(toggleSequence([]), null);
});

// ---------- the key engine hooks and the registry ----------

test('a chord prefix older than the sequence timeout starts over', () => {
  const e = new KeyEngine(desktopKeymap());
  assert.deepEqual(e.feed('g', 1000), { kind: 'pending' });
  assert.equal(e.feed('g', 1000 + SEQUENCE_TIMEOUT_MS - 1).kind, 'run');
  e.feed('g', 5000);
  assert.equal(e.expire(5000 + SEQUENCE_TIMEOUT_MS + 1), true);
  assert.equal(e.busy(), false);
  e.feed('g', 9000);
  assert.deepEqual(e.feed('a', 9000 + SEQUENCE_TIMEOUT_MS + 5), { kind: 'run', action: 'focus-composer', count: null, keys: 'a', arg: 'a' });
  e.feed('3', 20000);
  assert.equal(e.expire(30000), false);
  assert.equal(e.view().pending, '3');
});

test(':pending and :drain run ahead of the command table with their arguments', () => {
  const e = new KeyEngine(desktopKeymap());
  assert.deepEqual(e.runCommand('pending'), { kind: 'run', action: 'pending-messages', count: null, keys: ':pending' });
  assert.deepEqual(e.runCommand(`:pending withdraw ${ID}`), { kind: 'run', action: 'pending-messages', count: null, keys: ':pending', arg: `withdraw ${ID}` });
  assert.deepEqual(e.runCommand('drain status'), { kind: 'run', action: 'drain-restart', count: null, keys: ':drain', arg: 'status' });
  assert.throws(() => buildKeymap({ normal: [], reserved: [], commands: [{ name: 'quit', aliases: [], arg: 'none', title: 'q', description: '' }], keyBindings: {}, commandBindings: {}, prefixCommands: { quit: { action: 'x' } } }), /command table/);
});

test('gf1..gf9 open recent files; i/a/o/O pass their key; every input action has a rule', () => {
  assert.equal(Object.keys(INPUT_KEY_BINDINGS).length, 9);
  assert.deepEqual(INPUT_KEY_BINDINGS.gf9, { action: 'open-recent-file', arg: '9' });
  assert.deepEqual(Object.keys(INPUT_PREFIX_COMMANDS), ['pending', 'drain']);
  for (const a of INPUT_ACTIONS) assert.ok(INPUT_RULES[a.id], a.id);
  const e = new KeyEngine(desktopKeymap());
  for (const k of ['i', 'a', 'o', 'O']) assert.equal(e.feed(k).arg, k);
});

test('availability: i from the list opens the cursor session; draft tools need text', () => {
  const base = {
    open: null, cursorOnRow: false, rowCount: 1, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
    showFinished: false, modalOpen: false,
  };
  assert.match(unavailable('focus-composer', base), /Open a session first/);
  assert.equal(unavailable('focus-composer', { ...base, cursorOnRow: true }), null);
  const open = { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false };
  const s = { ...base, open, input: { recentFiles: 0, draft: false, submitOnEnter: true, back: false, forward: true } };
  assert.match(unavailable('compile-prompt', s), /empty/);
  assert.equal(unavailable('compile-prompt', { ...s, input: { ...s.input, draft: true } }), null);
  assert.match(unavailable('open-recent-file', s), /No recent files/);
  assert.match(unavailable('jump-back', s), /No older session/);
  assert.equal(unavailable('jump-forward', s), null);
  assert.match(unavailable('input-modal', { ...s, open: { ...open, readOnly: true } }), /read-only/);
});
