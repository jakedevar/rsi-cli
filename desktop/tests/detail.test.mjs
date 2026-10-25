import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  ACTIONS, appMenuModel, menuItems, contextMenuModel, keyHints, paletteEntries, tooltipText, unavailable,
} from '../dist/registry.js';
import { DETAIL_ACTIONS, DETAIL_COMMAND_BINDINGS, DETAIL_KEY_BINDINGS, DETAIL_RULES } from '../dist/actions/detail-spec.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import {
  SHOW_ALL, buildSwitch, checkFollowUp, checkTitle, chosen, effortChoices, focusAfter, foldTargets, foldWanted,
  isFoldable, isVisible, keepCursor, modelAllowed, moveModel, parseRating, pendingLabel, previewMove, previewScroll,
  overlayChord, ratingFromKey, routeDetailKey, sessionRating, stepEffort, stepEvent, stepRating, stepUserMessage, switchDecision,
  switchEfforts, toolGroups, tupleLabel, visibleSeqs, yankTarget,
} from '../dist/detail-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

let seq = 0;
const ev = (event_type, over = {}) => ({
  id: ++seq, session_id: 's', sequence: over.sequence ?? seq, event_type, role: null,
  created_at: '2026-10-18T00:00:00Z', content: 'text', ...over,
});
const user = (n, over = {}) => ev('Message', { sequence: n, role: 'User', ...over });
const asst = (n) => ev('Message', { sequence: n, role: 'Assistant' });
const call = (n, id) => ev('ToolUse', { sequence: n, tool_name: 'Bash', tool_use_id: id, tool_input: { cmd: 'ls' } });
const result = (n, id) => ev('ToolResult', { sequence: n, tool_name: 'Bash', tool_use_id: id });
const sys = (n) => ev('System', { sequence: n });
const think = (n) => ev('Thinking', { sequence: n });

/** A running open session with a few transcript events. */
const snap = (detail = {}, over = {}) => ({
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null,
  detail: { events: 5, shown: 5, userMessages: 2, cursor: false, focus: 'list', hideSystem: false, hideThinking: false, ...detail },
  ...over,
});
const idle = () => snap({}, { open: null, cursorOnRow: false, rowCount: 0 });

// ---------- registry ----------

test('every detail action is registered, listed in the palette and has a rule', () => {
  const ids = DETAIL_ACTIONS.map((a) => a.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const id of ids) {
    assert.ok(ACTIONS.some((a) => a.id === id), id);
    assert.equal(typeof DETAIL_RULES[id], 'function', id);
  }
  const entries = paletteEntries(snap(), 'other');
  for (const id of ids) assert.ok(entries.some((e) => e.id === id), `${id} is in the palette`);
  assert.deepEqual(Object.keys(DETAIL_RULES).sort(), [...ids].sort());
});

test('detail key bindings show up as key hints from the TUI notation', () => {
  assert.deepEqual(keyHints('user-next'), [']u']);
  assert.deepEqual(keyHints('user-prev'), ['[u']);
  assert.deepEqual(keyHints('event-next'), ['Shift-Down']);
  assert.deepEqual(keyHints('fold-toggle'), ['za']);
  assert.deepEqual(keyHints('fold-close-all'), ['zM']);
  assert.deepEqual(keyHints('toggle-system'), ['zs']);
  assert.deepEqual(keyHints('toggle-thinking'), ['zt']);
  assert.deepEqual(keyHints('yank'), ['yy']);
  assert.deepEqual(keyHints('rename-session'), ['F2']);
  assert.deepEqual(keyHints('switch-model'), ['<Space>mm']);
  assert.deepEqual(keyHints('prompt-preview'), ['p']);
  assert.deepEqual(keyHints('rate-session'), [':rate']);
  assert.equal(tooltipText('rename-session', 'other').split('\n')[0], 'Rename session… (F2)');
});

test('the key engine runs the detail actions for their TUI keys and commands', () => {
  const run = (keys) => {
    const e = new KeyEngine(desktopKeymap());
    let o;
    for (const k of keys) o = e.feed(k);
    return o;
  };
  assert.deepEqual(run([']', 'u']), { kind: 'run', action: 'user-next', count: null, keys: ']u' });
  assert.equal(run(['[', 'u']).action, 'user-prev');
  assert.equal(run(['z', 'o']).action, 'fold-open');
  assert.equal(run(['z', 'c']).action, 'fold-close');
  assert.equal(run(['z', 'a']).action, 'fold-toggle');
  assert.equal(run(['z', 'M']).action, 'fold-close-all');
  assert.equal(run(['z', 'R']).action, 'fold-open-all');
  assert.equal(run(['z', 's']).action, 'toggle-system');
  assert.equal(run(['z', 't']).action, 'toggle-thinking');
  assert.equal(run(['y', 'y']).action, 'yank');
  assert.equal(run(['F2']).action, 'rename-session');
  assert.equal(run(['p']).action, 'prompt-preview');
  assert.equal(run(['Space', 'm', 'm']).action, 'switch-model');
  const e = new KeyEngine(desktopKeymap());
  assert.deepEqual(e.runCommand('rate 7'), { kind: 'run', action: 'rate-session', count: null, keys: ':rate', arg: '7' });
  assert.equal(e.runCommand('r').action, 'rate-session');
  // Ctrl-M belongs to another slice (G2's navigation), not to the detail model switch.
  assert.equal(run(['C-m']).action, 'model-dropdown', 'Ctrl-M is the G2 default-model dropdown, not the detail model switch');
});

test('every detail key and command names a registered action', () => {
  const known = new Set(ACTIONS.map((a) => a.id));
  for (const [k, b] of [...Object.entries(DETAIL_KEY_BINDINGS), ...Object.entries(DETAIL_COMMAND_BINDINGS)]) {
    assert.ok(known.has(b.action), `${k} -> ${b.action}`);
  }
});

test('detail actions say why they are unavailable', () => {
  for (const id of ['event-next', 'user-next', 'fold-open', 'toggle-system', 'yank', 'rename-session', 'rate-session', 'switch-model']) {
    assert.match(unavailable(id, idle()), /Open a session first/, id);
  }
  assert.match(unavailable('prompt-preview', idle()), /No session to preview/);
  assert.equal(unavailable('prompt-preview', snap({}, { open: null })), null, 'a cursor row can be previewed');
  assert.match(unavailable('event-next', snap({ shown: 0 })), /no events/);
  assert.equal(unavailable('event-next', snap()), null);
  assert.match(unavailable('user-next', snap({ userMessages: 0 })), /no user messages/);
  assert.match(unavailable('fold-toggle', snap()), /Select an event first/);
  assert.equal(unavailable('fold-toggle', snap({ cursor: true })), null);
  assert.equal(unavailable('fold-close-all', snap()), null);
  const ro = snap({}, { open: { status: 'Archived', readOnly: true, pauseLevel: 'none', hasQuestion: false } });
  assert.match(unavailable('switch-model', ro), /read-only/);
  assert.equal(unavailable('rename-session', ro), null, 'an archived session can still be renamed');
  assert.match(unavailable('event-next', snap({}, { modalOpen: true })), /Close the open dialog/);
});

test('the menus and context menus offer the detail actions from the registry', () => {
  const s = snap({ cursor: true });
  const menus = appMenuModel('other', s);
  const find = (id) => menus.flatMap((m) => menuItems(m.items)).find((n) => n.id === `act:${id}`);
  const view = menuItems(menus.find((m) => m.id === 'view').items);
  for (const id of ['fold-open', 'fold-close', 'fold-toggle', 'fold-close-all', 'fold-open-all', 'toggle-system', 'toggle-thinking']) {
    assert.ok(view.some((n) => n.id === `act:${id}`), `View > ${id}`);
  }
  assert.equal(find('toggle-system').checked, true);
  assert.equal(appMenuModel('other', snap({ hideSystem: true }))
    .flatMap((m) => menuItems(m.items)).find((n) => n.id === 'act:toggle-system').checked, false);
  const go = menuItems(menus.find((m) => m.id === 'go').items).map((n) => n.id);
  for (const id of ['event-next', 'event-prev', 'user-next', 'user-prev']) assert.ok(go.includes(`act:${id}`), id);
  assert.ok(!go.includes('act:scroll-up'), 'the scroll keys stay out of the menu bar');
  const sess = menuItems(menus.find((m) => m.id === 'session').items).map((n) => n.id);
  for (const id of ['rename-session', 'rate-session', 'switch-model', 'prompt-preview', 'yank']) assert.ok(sess.includes(`act:${id}`), id);
  const row = contextMenuModel('session-row', s).filter((n) => n.kind === 'item').map((n) => n.id);
  for (const id of ['rename-session', 'rate-session', 'switch-model', 'prompt-preview']) assert.ok(row.includes(`ctx:${id}`), id);
});

test('every tauri command the detail adapter invokes is registered in the Rust handler list', () => {
  const ts = readFileSync(here('../src/actions/detail.ts'), 'utf8');
  const lib = readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8');
  const used = [...ts.matchAll(/invoke(?:<[^>]*>)?\('([a-z_]+)'/g)].map((m) => m[1]);
  assert.ok(used.length >= 5, used.join());
  for (const name of used) assert.match(lib, new RegExp(`detail::${name}\\b`), name);
});

// ---------- visibility and navigation ----------

test('visibility hides tool, system and thinking events independently', () => {
  const events = [user(1), call(2, 'a'), result(3, 'a'), sys(4), think(5), asst(6)];
  assert.deepEqual(visibleSeqs(events, SHOW_ALL), [1, 2, 3, 4, 5, 6]);
  assert.deepEqual(visibleSeqs(events, { ...SHOW_ALL, hideTools: true }), [1, 4, 5, 6]);
  assert.deepEqual(visibleSeqs(events, { ...SHOW_ALL, hideSystem: true }), [1, 2, 3, 5, 6]);
  assert.deepEqual(visibleSeqs(events, { ...SHOW_ALL, hideThinking: true }), [1, 2, 3, 4, 6]);
  assert.equal(isVisible(user(1), { hideTools: true, hideSystem: true, hideThinking: true }), true);
});

test('Shift-Down / Shift-Up step through shown events and stop at the ends', () => {
  const seqs = [1, 3, 4, 8];
  assert.equal(stepEvent(seqs, null, 1), 1, 'next with no cursor is the first');
  assert.equal(stepEvent(seqs, null, -1), 8, 'previous with no cursor is the last');
  assert.equal(stepEvent(seqs, 1, 1), 3);
  assert.equal(stepEvent(seqs, 4, 1), 8);
  assert.equal(stepEvent(seqs, 8, 1), 8, 'does not wrap forward');
  assert.equal(stepEvent(seqs, 3, -1), 1);
  assert.equal(stepEvent(seqs, 1, -1), 1, 'does not wrap back');
  assert.equal(stepEvent(seqs, 2, 1), 3, 'a hidden cursor steps to the next shown event');
  assert.equal(stepEvent(seqs, 2, -1), 1);
  assert.equal(stepEvent([], 1, 1), null);
});

test(']u / [u jump between user messages and wrap', () => {
  const events = [user(1), asst(2), user(5), call(6, 'a'), user(9)];
  assert.equal(stepUserMessage(events, SHOW_ALL, null, 1), 1);
  assert.equal(stepUserMessage(events, SHOW_ALL, null, -1), 9, 'the first backward jump starts at the newest');
  assert.equal(stepUserMessage(events, SHOW_ALL, 1, 1), 5);
  assert.equal(stepUserMessage(events, SHOW_ALL, 5, 1), 9);
  assert.equal(stepUserMessage(events, SHOW_ALL, 9, 1), 1, 'wraps to the oldest');
  assert.equal(stepUserMessage(events, SHOW_ALL, 5, -1), 1);
  assert.equal(stepUserMessage(events, SHOW_ALL, 1, -1), 9, 'wraps to the newest');
  assert.equal(stepUserMessage([asst(1)], SHOW_ALL, null, 1), null);
  assert.equal(stepUserMessage(events, SHOW_ALL, 4, 1), 5, 'from a non-user event');
});

test('the cursor survives while its event exists', () => {
  const events = [user(1), asst(2)];
  assert.equal(keepCursor(events, 2), 2);
  assert.equal(keepCursor(events, 7), null);
  assert.equal(keepCursor(events, null), null);
});

// ---------- tool groups and folds ----------

test('a tool call pairs with its result by id, wherever the result sits', () => {
  const events = [call(1, 'a'), call(2, 'b'), result(3, 'b'), asst(4), result(5, 'a'), call(6, undefined), result(7, undefined)];
  const g = toolGroups(events);
  assert.deepEqual(g.get(1).members, [1, 5]);
  assert.deepEqual(g.get(5).members, [1, 5]);
  assert.equal(g.get(5).leader, 1);
  assert.deepEqual(g.get(2).members, [2, 3]);
  assert.equal(g.has(4), false);
  assert.equal(g.has(6), false, 'an event without an id is never paired by adjacency');
  assert.equal(g.has(7), false);
});

test('fold targets are the whole group for a tool event, else the event itself', () => {
  const events = [call(1, 'a'), user(2), result(3, 'a'), call(4, null)];
  assert.deepEqual(foldTargets(events, 1), [1, 3]);
  assert.deepEqual(foldTargets(events, 3), [1, 3]);
  assert.deepEqual(foldTargets(events, 2), [2]);
  assert.deepEqual(foldTargets(events, 4), [4]);
  assert.deepEqual(foldTargets(events, null), []);
  assert.deepEqual(foldTargets(events, 99), []);
});

test('zo opens, zc closes, za opens unless everything is already open', () => {
  assert.equal(foldWanted('open', [true, false]), true);
  assert.equal(foldWanted('close', [true, true]), false);
  assert.equal(foldWanted('toggle', [false, false]), true);
  assert.equal(foldWanted('toggle', [true, false]), true, 'a half-open group opens as one unit');
  assert.equal(foldWanted('toggle', [true, true]), false);
});

test('tool, thinking and long messages are foldable; short messages are not', () => {
  assert.equal(isFoldable(call(1, 'a')), true);
  assert.equal(isFoldable(result(2, 'a')), true);
  assert.equal(isFoldable(think(3)), true);
  assert.equal(isFoldable(user(4)), false);
  assert.equal(isFoldable(asst(5)), false);
  assert.equal(isFoldable({ ...asst(6), content: 'word '.repeat(1400) }), true);
});

// ---------- yank and focus ----------

test('yy copies the selected event when the keys are in the transcript, else the session ID', () => {
  assert.equal(yankTarget('transcript', 4), 'event');
  assert.equal(yankTarget('transcript', null), 'session-id');
  assert.equal(yankTarget('list', 4), 'session-id');
  assert.equal(focusAfter('event-next', 'list'), 'transcript');
  assert.equal(focusAfter('fold-open', 'list'), 'list', 'a fold key keeps the pane that has the keys');
  assert.equal(focusAfter('fold-open', 'transcript'), 'transcript');
  assert.equal(focusAfter('cursor-down', 'transcript'), 'list');
  assert.equal(focusAfter('open', 'transcript'), 'list');
  assert.equal(focusAfter('rename-session', 'transcript'), 'transcript');
  assert.equal(focusAfter('yank', 'list'), 'list');
});

// ---------- input bar priority ----------

const key = (k, over = {}) => ({ key: k, ...over });
const env = (over = {}) => ({
  composerFocused: false, draft: '', modalOpen: false, typingElsewhere: false, sessionOpen: true, ...over,
});

test('outside the message box Shift-Down / Shift-Up move the event cursor and Up / Down scroll', () => {
  assert.deepEqual(routeDetailKey(key('ArrowDown', { shiftKey: true }), env()), { kind: 'action', action: 'event-next' });
  assert.deepEqual(routeDetailKey(key('ArrowUp', { shiftKey: true }), env()), { kind: 'action', action: 'event-prev' });
  assert.deepEqual(routeDetailKey(key('ArrowDown'), env()), { kind: 'action', action: 'scroll-down' });
  assert.deepEqual(routeDetailKey(key('ArrowUp'), env()), { kind: 'action', action: 'scroll-up' });
  assert.equal(routeDetailKey(key('j'), env()).kind, 'pass');
});

test('the message box owns its keys; Esc hands the keyboard back; Up / Down scroll only a one-line draft', () => {
  const box = (draft) => env({ composerFocused: true, draft });
  assert.equal(routeDetailKey(key('j'), box('hello')).kind, 'pass');
  assert.equal(routeDetailKey(key('ArrowDown', { shiftKey: true }), box('')).kind, 'pass', 'Shift-arrows select text while typing');
  assert.deepEqual(routeDetailKey(key('Escape'), box('')), { kind: 'blur-composer' });
  assert.deepEqual(routeDetailKey(key('Escape'), box('a draft')), { kind: 'blur-composer' });
  assert.deepEqual(routeDetailKey(key('ArrowUp'), box('')), { kind: 'action', action: 'scroll-up' });
  assert.deepEqual(routeDetailKey(key('ArrowDown'), box('one line')), { kind: 'action', action: 'scroll-down' });
  assert.equal(routeDetailKey(key('ArrowUp'), box('two\nlines')).kind, 'pass', 'the caret moves in a multi-line draft');
  assert.equal(routeDetailKey(key('ArrowUp', { altKey: true }), box('')).kind, 'pass');
});

test('dialogs, other text fields, no open session and held modifiers pass every key through', () => {
  const down = key('ArrowDown', { shiftKey: true });
  assert.equal(routeDetailKey(down, env({ modalOpen: true })).kind, 'pass');
  assert.equal(routeDetailKey(down, env({ typingElsewhere: true })).kind, 'pass');
  assert.equal(routeDetailKey(down, env({ sessionOpen: false })).kind, 'pass');
  assert.equal(routeDetailKey(key('ArrowDown', { ctrlKey: true }), env()).kind, 'pass');
  assert.equal(routeDetailKey(key('ArrowDown', { metaKey: true }), env()).kind, 'pass');
  assert.equal(routeDetailKey(key('ArrowDown', { isComposing: true }), env()).kind, 'pass');
  assert.equal(routeDetailKey(key('ArrowDown', { defaultPrevented: true }), env()).kind, 'pass');
  assert.equal(routeDetailKey(key('Escape'), env({ composerFocused: true, modalOpen: true })).kind, 'pass');
});

// ---------- rename ----------

test('a title is trimmed, one line and at most 200 characters', () => {
  assert.deepEqual(checkTitle('  Fix the build  '), { ok: true, value: 'Fix the build' });
  assert.equal(checkTitle('   ').ok, false);
  assert.equal(checkTitle('two\nlines').ok, false);
  assert.equal(checkTitle('x'.repeat(200)).ok, true);
  assert.equal(checkTitle('x'.repeat(201)).ok, false);
  assert.equal(checkTitle('é'.repeat(200)).ok, true, 'counts characters, not bytes');
});

// ---------- rating ----------

test(':rate takes a whole number 1-10, or opens the overlay without one', () => {
  assert.deepEqual(parseRating(undefined), { ok: true, value: null });
  assert.deepEqual(parseRating(''), { ok: true, value: null });
  assert.deepEqual(parseRating('7'), { ok: true, value: 7 });
  assert.deepEqual(parseRating(' 10 '), { ok: true, value: 10 });
  for (const bad of ['0', '11', 'abc', '-1', '7.5', '1e1']) assert.equal(parseRating(bad).ok, false, bad);
  assert.match(parseRating('abc').why, /Use :rate <1-10>/);
});

test('the rating overlay: 1-9 pick, 0 picks 10, h/l step and clamp', () => {
  assert.equal(ratingFromKey('1'), 1);
  assert.equal(ratingFromKey('9'), 9);
  assert.equal(ratingFromKey('0'), 10);
  assert.equal(ratingFromKey('a'), null);
  assert.equal(stepRating(null, 1), 1);
  assert.equal(stepRating(null, -1), 1);
  assert.equal(stepRating(5, 1), 6);
  assert.equal(stepRating(10, 1), 10);
  assert.equal(stepRating(1, -1), 1);
  assert.equal(sessionRating(7), 7);
  assert.equal(sessionRating(0), null);
  assert.equal(sessionRating(null), null);
  assert.equal(sessionRating(11), null);
});

// ---------- model / effort switch ----------

const options = (over = {}) => ({
  session_id: 's', provider: 'Claude', model: 'claude-opus-5-5', effort: 'high', model_invocation_id: 'inv-1',
  switchable: true, keeps_context: true, context_note: 'Keeps the conversation.', model_allowlist: [], pending: null, ...over,
});
const found = {
  models: [['claude-opus-5-5', 'Opus 5.5'], ['claude-sonnet-5-5', 'Sonnet 5.5'], ['claude-haiku-5', 'Haiku 5']],
  effort_capabilities: [
    { model: 'claude-opus-5-5', supported_efforts: ['low', 'medium', 'high', 'xhigh', 'max'], default_effort: 'xhigh' },
    { model: 'claude-sonnet-5-5', supported_efforts: ['low', 'medium', 'high'], default_effort: 'high' },
  ],
};

test('the picker seeds from the current model and effort', () => {
  const st = buildSwitch(options(), found);
  assert.deepEqual(st.models.map((m) => m.id), ['claude-opus-5-5', 'claude-sonnet-5-5', 'claude-haiku-5']);
  assert.equal(st.modelIndex, 0);
  assert.deepEqual(st.efforts, [null, 'low', 'medium', 'high', 'xhigh', 'max']);
  assert.equal(st.efforts[st.effortIndex], 'high');
  assert.deepEqual(chosen(st), { model: 'claude-opus-5-5', effort: 'high' });
});

test('the current model always has a row, even when discovery omits it or fails', () => {
  const st = buildSwitch(options({ model: 'claude-custom' }), found);
  assert.equal(st.models[0].id, 'claude-custom');
  assert.equal(st.modelIndex, 0);
  const alone = buildSwitch(options(), null);
  assert.deepEqual(alone.models.map((m) => m.id), ['claude-opus-5-5']);
  assert.deepEqual(alone.efforts, [null, ...switchEfforts('Claude')]);
});

test('the operator allowlist narrows the models; none left means no picker', () => {
  const st = buildSwitch(options({ model_allowlist: ['claude-sonnet-5-5', 'Claude-Haiku-5'] }), found);
  assert.deepEqual(st.models.map((m) => m.id), ['claude-sonnet-5-5', 'claude-haiku-5']);
  assert.equal(buildSwitch(options({ model: null, model_allowlist: ['nothing-matches'] }), found), null);
  assert.equal(modelAllowed([], 'Claude', 'anything'), true);
  assert.equal(modelAllowed(undefined, 'Claude', 'anything'), true);
  assert.equal(modelAllowed(['claude-opus-5-5'], 'Claude', 'claude-opus-5-5[1m]'), true, 'the Claude 1M tag is ignored');
  assert.equal(modelAllowed(['claude-opus-5-5'], 'OpenRouter', 'claude-opus-5-5[1m]'), false);
});

test('moving the model re-derives the efforts and keeps the highlighted one when offered', () => {
  let st = buildSwitch(options(), found);
  st = moveModel(st, 1);
  assert.equal(chosen(st).model, 'claude-sonnet-5-5');
  assert.deepEqual(st.efforts, [null, 'low', 'medium', 'high']);
  assert.equal(chosen(st).effort, 'high', 'high is offered again');
  st = stepEffort(st, 1);
  assert.equal(chosen(st).effort, 'high', 'clamped at the top of the ladder');
  st = stepEffort(st, -1);
  assert.equal(chosen(st).effort, 'medium');
  st = stepEffort(st, -1); st = stepEffort(st, -1); st = stepEffort(st, -1);
  assert.equal(chosen(st).effort, null, 'the bottom row is the model default');
  st = moveModel(st, 0);
  assert.equal(chosen(st).effort, null, 'the default effort carries over');
  assert.equal(moveModel(st, 99).modelIndex, 2, 'clamped');
  assert.equal(moveModel(st, -5).modelIndex, 0);
  const high = moveModel(buildSwitch(options({ effort: 'max' }), found), 1);
  assert.equal(chosen(high).effort, null, 'max is not offered by Sonnet, so the default row');
});

test('effort rows are limited to what the provider can switch', () => {
  assert.deepEqual(switchEfforts('Antigravity'), ['low', 'medium', 'high']);
  assert.deepEqual(switchEfforts('Bedrock'), []);
  assert.deepEqual(effortChoices('Antigravity', 'm', [{ model: 'm', supported_efforts: ['low', 'xhigh'] }]), [null, 'low']);
  assert.deepEqual(effortChoices('Bedrock', 'm', []), [null]);
});

test('Enter does nothing for the running tuple, and queues anything else', () => {
  const st = buildSwitch(options(), found);
  assert.equal(switchDecision(st), 'unchanged');
  assert.equal(switchDecision(stepEffort(st, 1)), 'queue');
  assert.equal(switchDecision(moveModel(st, 1)), 'queue');
  assert.equal(switchDecision(buildSwitch(options({ pending: { model: 'claude-haiku-5', effort: null } }), found)), 'queue',
    'a queued switch can be replaced by re-picking the running tuple');
  assert.equal(switchDecision(buildSwitch(options({ model_invocation_id: null }), found)), 'no-fence');
  assert.equal(tupleLabel('claude-opus-5-5', 'xhigh'), 'opus-5-5 · xhigh');
  assert.equal(tupleLabel('claude-opus-5-5', null), 'opus-5-5 · default effort');
  assert.equal(pendingLabel({ model: 'claude-opus-5-5', effort: 'max' }), '→ opus-5-5 · max');
  assert.equal(pendingLabel(null), null);
});

// ---------- prompt preview and continue ----------

test('the prompt preview follows j/k/G over the session list and clamps', () => {
  const ids = ['a', 'b', 'c'];
  assert.equal(previewMove(ids, 'a', 'next'), 'b');
  assert.equal(previewMove(ids, 'c', 'next'), 'c');
  assert.equal(previewMove(ids, 'b', 'prev'), 'a');
  assert.equal(previewMove(ids, 'a', 'prev'), 'a');
  assert.equal(previewMove(ids, 'a', 'last'), 'c');
  assert.equal(previewMove(ids, null, 'next'), 'a');
  assert.equal(previewMove(ids, null, 'prev'), 'c');
  assert.equal(previewMove([], 'a', 'next'), null);
  assert.equal(previewScroll(0, -1), 0);
  assert.equal(previewScroll(5, 1), 15);
  assert.equal(previewScroll(15, -1), 5);
});

test('the continue prompt refuses an empty follow-up and keeps the text otherwise', () => {
  assert.equal(checkFollowUp('  \n ').ok, false);
  assert.deepEqual(checkFollowUp('  fix it\n\n'), { ok: true, value: '  fix it' });
});

// ---------- overlay chords ----------

test('Space N and Space ; leave an overlay for a new session prompt or the palette', () => {
  assert.deepEqual(overlayChord(false, ' '), { armed: true, run: null });
  assert.deepEqual(overlayChord(true, 'N'), { armed: false, run: 'launch' });
  assert.deepEqual(overlayChord(true, ';'), { armed: false, run: 'palette' });
  assert.deepEqual(overlayChord(true, 'x'), { armed: false, run: null }, 'another key drops the Space');
  assert.deepEqual(overlayChord(false, 'N'), { armed: false, run: null }, 'N alone is not the chord');
  assert.deepEqual(overlayChord(true, ' '), { armed: true, run: null });
});

test('G2: without a discovered ladder the effort rows follow the compiled per-model ladder', async () => {
  const { effortLadder, parseModelVersion } = await import('../dist/detail-model.js');
  assert.deepEqual(effortLadder('claude-opus-5-5'), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(effortLadder('claude-sonnet-5'), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(effortLadder('claude-opus-4-7'), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(effortLadder('claude-opus-4-6'), ['low', 'medium', 'high', 'max']);
  assert.deepEqual(effortLadder('claude-opus-4-5-20251101'), ['low', 'medium', 'high']);
  assert.deepEqual(effortLadder('claude-haiku-4-5'), []);
  assert.deepEqual(effortLadder('claude-sonnet-4-20250514'), []);
  assert.deepEqual(effortLadder('gpt-6-astra'), ['low', 'medium', 'high', 'xhigh', 'max', 'ultra']);
  assert.deepEqual(effortLadder('openai.gpt-5.6-luna'), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(effortLadder('gpt-5.5'), ['low', 'medium', 'high', 'xhigh']);
  assert.deepEqual(effortLadder('gpt-5.9-new'), ['low', 'medium', 'high', 'xhigh'], 'unknown Codex reasoning models keep the legacy ladder');
  assert.deepEqual(parseModelVersion('claude-sonnet-5'), { family: 'sonnet', major: 5, minor: 0 });
  assert.deepEqual(effortChoices('Claude', 'claude-opus-4-6', []), [null, 'low', 'medium', 'high', 'max']);
  assert.deepEqual(effortChoices('Codex', 'gpt-6-astra', []), [null, 'low', 'medium', 'high', 'xhigh', 'max', 'ultra']);
  assert.deepEqual(effortChoices('Claude', 'claude-haiku-4-5', []), [null]);
  assert.deepEqual(effortChoices('Claude', 'claude-haiku-4-5', [{ model: 'claude-haiku-4-5', supported_efforts: ['low'] }]), [null, 'low'], 'discovery wins');
});
