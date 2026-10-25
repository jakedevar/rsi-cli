// Find in the open conversation (local #307): the matches cover every loaded event (not the rendered window), the
// current match and its window, the count line, the chord, the registry rules, and the guarantee that the find never
// reads or writes the session-list filter.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  REVEAL_CONTEXT, eventSearchText, findLabel, findMatches, isFindChord, keepCurrent, limitToReveal, occurrences,
  splitMatches, stepFind,
} from '../dist/find-model.js';
import { ACTIONS, keyHints, unavailable } from '../dist/registry.js';

const here = dirname(fileURLToPath(import.meta.url));
const SHOW_ALL = { hideTools: false, hideSystem: false, hideThinking: false };

const msg = (sequence, content, role = 'Assistant') => ({
  id: sequence, session_id: 's', sequence, created_at: '2026-10-20T00:00:00Z', event_type: 'Message', role, content,
});
const tool = (sequence, content) => ({
  id: sequence, session_id: 's', sequence, created_at: '2026-10-20T00:00:00Z', event_type: 'ToolResult', role: 'Tool', content,
});
/** 1200 events; the needle sits in three (indices 2, 1149 and 1189), the first far behind a 400-event window. */
function long() {
  const events = Array.from({ length: 1200 }, (_, i) => msg(i + 1, `Message ${i + 1}: lorem ipsum`));
  events[2] = msg(3, 'An early note with Needle inside');
  events[1149] = msg(1150, 'A reply with **needle** in bold');
  events[1189] = tool(1190, 'result: NEEDLE again');
  return events;
}

test('a match behind "Show earlier" is found: the list covers every loaded event, not the last 400', () => {
  const events = long();
  assert.deepEqual(findMatches(events, 'needle', SHOW_ALL), [2, 1149, 1189]);
  assert.deepEqual(findMatches(events, 'Message 12:', SHOW_ALL), [11], '"Message 12:" sits behind the window and is found');
  assert.deepEqual(findMatches(events, '  ', SHOW_ALL), [], 'a blank needle finds nothing');
});

test('hidden events are not matches: hidden tool calls cannot be scrolled to', () => {
  const events = long();
  assert.deepEqual(findMatches(events, 'needle', { ...SHOW_ALL, hideTools: true }), [2, 1149]);
});

test('tool input and the tool name are searched as the transcript shows them', () => {
  const e = { ...tool(5, ''), event_type: 'ToolUse', role: 'Assistant', tool_name: 'Bash', tool_input: { command: 'rg pattern src/' } };
  assert.match(eventSearchText(e), /pattern/);
  assert.deepEqual(findMatches([e], 'bash', SHOW_ALL), [0]);
});

test('stepping goes forward and back, wraps at both ends, and starts at the ends from nothing', () => {
  const m = [2, 1149, 1189];
  assert.equal(stepFind(m, null, 1), 2);
  assert.equal(stepFind(m, null, -1), 1189);
  assert.equal(stepFind(m, 2, 1), 1149);
  assert.equal(stepFind(m, 1189, 1), 2, 'wraps forward');
  assert.equal(stepFind(m, 2, -1), 1189, 'wraps back');
  assert.equal(stepFind(m, 1149, -1), 2);
  assert.equal(stepFind([], 3, 1), null);
});

test('the current match survives live events and a longer needle', () => {
  assert.equal(keepCurrent([2, 1149, 1189], null), 2);
  assert.equal(keepCurrent([2, 1149, 1189], 1149), 1149, 'still a match: stay');
  assert.equal(keepCurrent([2, 1189], 1149), 1189, 'gone: the next one on');
  assert.equal(keepCurrent([2], 1149), 2, 'nothing after: the first');
  assert.equal(keepCurrent([], 1149), null);
});

test('the count line reads "3 of 17", "No match" and nothing for a blank query', () => {
  assert.equal(findLabel([2, 1149, 1189], 1149, 'needle'), '2 of 3');
  assert.equal(findLabel([2, 1149, 1189], 1189, 'needle'), '3 of 3');
  assert.equal(findLabel([], null, 'needle'), 'No match');
  assert.equal(findLabel([], null, ' '), '');
});

test('the render window grows to include a match behind "Show earlier", and only then', () => {
  assert.equal(limitToReveal(1200, 1189, 400), 400, 'already shown');
  assert.equal(limitToReveal(1200, 800, 400), 400, 'the 400th from the end is shown');
  assert.equal(limitToReveal(1200, 2, 400), 1200, 'capped at the conversation');
  assert.equal(limitToReveal(5000, 4600, 400), 400);
  const limit = limitToReveal(5000, 100, 400);
  assert.equal(limit, 5000 - 100 + REVEAL_CONTEXT);
  assert.ok(5000 - limit < 100, 'the match is inside the window');
});

test('occurrences are located case-insensitively and cut a text into plain and marked pieces', () => {
  assert.deepEqual(occurrences('Needle and NEEDLE', 'needle'), [[0, 6], [11, 17]]);
  assert.deepEqual(splitMatches('a needle b', 'needle'), [
    { text: 'a ', hit: false }, { text: 'needle', hit: true }, { text: ' b', hit: false },
  ]);
  assert.deepEqual(splitMatches('plain', 'zzz'), [{ text: 'plain', hit: false }]);
  assert.deepEqual(splitMatches('plain', ''), [{ text: 'plain', hit: false }]);
  assert.equal(splitMatches('xNeedlex', 'needle').map((p) => p.text).join(''), 'xNeedlex', 'the pieces rebuild the text');
});

test('the find chord is Cmd-F on macOS and Ctrl-F elsewhere, never with Alt or Shift', () => {
  const f = (over) => ({ key: 'f', code: 'KeyF', ...over });
  assert.ok(isFindChord(f({ metaKey: true }), 'mac'));
  assert.ok(!isFindChord(f({ ctrlKey: true }), 'mac'), 'physical Ctrl-F stays the TUI page-down on macOS');
  assert.ok(isFindChord(f({ ctrlKey: true }), 'other'));
  assert.ok(!isFindChord(f({ metaKey: true }), 'other'));
  assert.ok(!isFindChord(f({ metaKey: true, shiftKey: true }), 'mac'));
  assert.ok(!isFindChord(f({ metaKey: true, altKey: true }), 'mac'));
  assert.ok(!isFindChord(f({ metaKey: true, isComposing: true }), 'mac'));
  assert.ok(!isFindChord({ key: 'g', code: 'KeyG', metaKey: true }, 'mac'));
  assert.ok(!isFindChord(f({}), 'mac'));
});

const open = { status: 'Completed', readOnly: false, pauseLevel: 'none', hasQuestion: false };
const snap = (over = {}) => ({
  open, cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: false, hideTools: false,
  showFinished: false, modalOpen: false, event: null, ...over,
});

test('Find in conversation is an action with Ctrl-F as its key, available whenever a session is open', () => {
  const a = ACTIONS.find((x) => x.id === 'find-in-conversation');
  assert.ok(a, 'registered');
  assert.deepEqual(keyHints('find-in-conversation'), ['Ctrl-F']);
  assert.equal(unavailable('find-in-conversation', snap()), null, 'works with Vim keys off');
  assert.match(unavailable('find-in-conversation', snap({ open: null })), /Open a session first/);
});

test('n and N step an active conversation find, with or without a list filter', () => {
  const list = (over) => ({
    cursorFoldable: false, cursorHasParent: false, foldable: 0, paneDetail: false, attentionQueue: 0,
    search: '', detailSearchable: false, projects: 0, labelGroups: false, openHasParent: false, ...over,
  });
  assert.match(unavailable('search-next', snap({ list: list({}) })), /search a conversation/);
  assert.equal(unavailable('search-next', snap({ list: list({ findActive: true }) })), null);
  assert.equal(unavailable('search-prev', snap({ list: list({ findActive: true }) })), null);
  assert.equal(unavailable('search-next', snap({ list: list({ search: 'x' }) })), null);
});

test('the find never reads or writes the session-list filter box', () => {
  const src = readFileSync(join(here, '../src/actions/find-run.ts'), 'utf8');
  assert.ok(!/getElementById\(\s*['"]search['"]/.test(src) && !/\$(?:<[^>]*>)?\(\s*['"]search['"]/.test(src), 'find-run.ts touches #search');
  assert.ok(!/renderSessionList|refreshSessions|setShowFinished/.test(src), 'find-run.ts redraws the session list');
  const run = readFileSync(join(here, '../src/actions/sessionlist-run.ts'), 'utf8');
  assert.ok(!/querySelectorAll<HTMLElement>\('\.ev'\)/.test(run), 'the list filter no longer matches transcript nodes');
});
