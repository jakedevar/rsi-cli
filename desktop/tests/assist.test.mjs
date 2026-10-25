// Assist overlays (hub #3015 slice 13B): registry entries and keys, the pure
// rules of memory search, the AI command / chat, the dialectic and the card
// editor, and the Tauri commands' wiring (request shapes are pinned by the
// Rust tests in src-tauri/src/assist.rs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, keyHints, paletteEntries, unavailable } from '../dist/registry.js';
import { ASSIST_ACTIONS, ASSIST_RULES, isAssistAction } from '../dist/actions/assist.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import {
  MAX_FACTS, activeProject, addFact, ask, askPrefill, assistRefusal, cancelEdit, cardNavKey, cardTitle, commitEdit,
  escapeAction, factsToSave, generatedText, memoryDetail, memoryHeader, memoryQueryToSend, moveSelection, openCard,
  parseCardCommand, parseMemoryResults, parseQueryResponse, previewKey, qCloses, questionToSend, reply, sourceLine,
} from '../dist/assist-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const snap = (over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, ...over,
});

test('the assist actions are registered with one rule each and reach the palette', () => {
  const ids = ASSIST_ACTIONS.map((a) => a.id);
  assert.deepEqual(ids, ['memory-search', 'dialectic', 'card', 'card-user', 'ai-command', 'ai-chat']);
  for (const id of ids) {
    assert.ok(ACTIONS.some((a) => a.id === id), id);
    assert.equal(typeof ASSIST_RULES[id], 'function', id);
    assert.ok(isAssistAction(id));
  }
  const palette = paletteEntries(snap(), 'other').map((e) => e.id);
  for (const id of ids) assert.ok(palette.includes(id), id);
  assert.deepEqual(keyHints('memory-search'), ['<Space>M']);
  assert.deepEqual(keyHints('dialectic'), [':ask']);
  assert.deepEqual(keyHints('card'), [':card']);
});

test('<Space>M, :ask and :card reach their actions with the argument', () => {
  const e = new KeyEngine(desktopKeymap());
  assert.equal(e.feed('Space').kind, 'pending');
  assert.deepEqual(e.feed('M'), { kind: 'run', action: 'memory-search', count: null, keys: '<Space>M' });
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('ask').action, 'dialectic');
  assert.deepEqual(new KeyEngine(desktopKeymap()).runCommand('ask what changed?'),
    { kind: 'run', action: 'dialectic', count: null, keys: ':ask', arg: 'what changed?' });
  assert.deepEqual(new KeyEngine(desktopKeymap()).runCommand('card user add Likes tea'),
    { kind: 'run', action: 'card', count: null, keys: ':card', arg: 'user add Likes tea' });
});

test('the AI actions need a draft; the overlays are always available', () => {
  assert.equal(unavailable('memory-search', snap()), null);
  assert.equal(unavailable('dialectic', snap()), null);
  assert.equal(unavailable('card-user', snap()), null);
  assert.match(unavailable('ai-command', snap()), /Type a message/);
  assert.match(unavailable('ai-chat', snap({ assist: { draft: false } })), /Type a message/);
  assert.equal(unavailable('ai-command', snap({ assist: { draft: true } })), null);
  assert.equal(unavailable('ai-chat', snap({ assist: { draft: true } })), null);
  assert.equal(assistRefusal('command', '  '), 'No text to transform');
  assert.equal(assistRefusal('chat', ''), 'No text to chat about');
  assert.equal(assistRefusal('chat', 'hi'), null);
});

test('memory results parse defensively and render the TUI header', () => {
  const rows = parseMemoryResults([
    { path: 'notes/rust.md', start_line: 3, end_line: 9, score: 0.8712, snippet: 'ratatui', source: 'memory' },
    { path: 'one.md', start_line: 4, end_line: 4, score: 1, snippet: '', source: '' },
    { nope: true }, null, 'x',
  ]);
  assert.equal(rows.length, 2);
  assert.equal(memoryHeader(rows[0]), 'notes/rust.md [0.87]');
  assert.equal(memoryDetail(rows[0]), 'lines 3-9 · memory');
  assert.equal(memoryDetail(rows[1]), 'line 4');
  assert.deepEqual(parseMemoryResults({ results: [] }), []);
  // The TUI searches on every edit and clears on an empty query (spaces are sent; rsid answers []).
  assert.equal(memoryQueryToSend(''), null);
  assert.equal(memoryQueryToSend('ru'), 'ru');
  assert.equal(moveSelection(3, 2, 1), 2);
  assert.equal(moveSelection(3, 0, -1), 0);
  assert.equal(moveSelection(0, 0, 1), 0);
});

test('chat and dialectic keys: Esc clears then closes, q closes only an empty input', () => {
  assert.equal(escapeAction('draft', false), 'clear');
  assert.equal(escapeAction('', false), 'close');
  assert.equal(escapeAction('draft', true), 'close', 'Esc closes while a request runs');
  assert.equal(qCloses('', false), true);
  assert.equal(qCloses('ab', false), false);
  assert.equal(questionToSend('  why? ', false), 'why?');
  assert.equal(questionToSend('   ', false), null);
  assert.equal(questionToSend('why', true), null);
});

test('a conversation keeps the question, the answer and errors like the TUI', () => {
  let turns = ask([], 'first?');
  assert.deepEqual(turns, [{ role: 'user', content: 'first?' }]);
  turns = reply(turns, generatedText({ text: '  An answer.\n' }));
  assert.deepEqual(turns[1], { role: 'assistant', content: 'An answer.' });
  turns = reply(ask(turns, 'second?'), { ok: false, error: 'daemon error: down' });
  assert.deepEqual(turns[3], { role: 'assistant', content: 'Error: daemon error: down' });
  assert.deepEqual(generatedText({ text: '   ' }), { ok: false, error: 'Model returned empty response' });
  assert.deepEqual(generatedText(null), { ok: false, error: 'Model returned empty response' });
});

test('the dialectic answer and sources parse, and :ask fills the input', () => {
  const r = parseQueryResponse({ answer: 'Yes.', sources: [{ kind: 'memory', label: 'notes.md', detail: '0.91' }, { label: 'x' }, 7], tool_calls: 2 });
  assert.equal(r.answer, 'Yes.');
  assert.equal(r.toolCalls, 2);
  assert.deepEqual(r.sources.map(sourceLine), ['[memory] notes.md (0.91)', '[source] x']);
  assert.equal(parseQueryResponse({ sources: [] }), null);
  assert.equal(askPrefill(undefined), '');
  assert.equal(askPrefill('  what changed? '), 'what changed?');
});

test('the AI command preview: a accepts, d or Esc discards', () => {
  assert.equal(previewKey('a'), 'accept');
  assert.equal(previewKey('d'), 'discard');
  assert.equal(previewKey('Escape'), 'discard');
  assert.equal(previewKey('x'), 'none');
});

test(':card parses exactly like the TUI', () => {
  assert.deepEqual(parseCardCommand(undefined), { kind: 'open', who: 'project' });
  assert.deepEqual(parseCardCommand(''), { kind: 'open', who: 'project' });
  assert.deepEqual(parseCardCommand('user'), { kind: 'open', who: 'user' });
  assert.deepEqual(parseCardCommand('add Uses Rust with ratatui'), { kind: 'add', who: 'project', fact: 'Uses Rust with ratatui' });
  assert.deepEqual(parseCardCommand('add "Quoted fact"'), { kind: 'add', who: 'project', fact: 'Quoted fact' });
  assert.deepEqual(parseCardCommand('user add Prefers short answers'), { kind: 'add', who: 'user', fact: 'Prefers short answers' });
  assert.equal(parseCardCommand('add ""').kind, 'invalid');
  assert.deepEqual(parseCardCommand('user something'), { kind: 'open', who: 'user' });
  assert.deepEqual(parseCardCommand('whatever'), { kind: 'open', who: 'project' });
  const full = Array.from({ length: MAX_FACTS }, (_, i) => `f${i}`);
  assert.equal(addFact(full, 'one more'), null);
  assert.deepEqual(addFact(['a'], 'b'), ['a', 'b']);
});

test('card navigation: move, jump, reorder (saved) and dd delete (saved)', () => {
  let st = openCard(['a', 'b', 'c']);
  const key = (k, mods = {}) => { const r = cardNavKey(st, { key: k, ...mods }); st = r.st; return r.effect; };
  assert.equal(key('j'), 'none'); assert.equal(st.selected, 1);
  assert.equal(key('G'), 'none'); assert.equal(st.selected, 2);
  assert.equal(key('j'), 'none'); assert.equal(st.selected, 2, 'clamped');
  assert.equal(key('g'), 'none'); assert.equal(st.selected, 0);
  assert.equal(key('J'), 'save'); assert.deepEqual(st.facts, ['b', 'a', 'c']); assert.equal(st.selected, 1);
  assert.equal(key('K'), 'save'); assert.deepEqual(st.facts, ['a', 'b', 'c']); assert.equal(st.selected, 0);
  assert.equal(key('K'), 'none', 'K at the top does nothing');
  assert.equal(key('G'), 'none');
  assert.equal(key('d'), 'none'); assert.equal(st.pendingDelete, true);
  assert.equal(key('d'), 'save'); assert.deepEqual(st.facts, ['a', 'b']); assert.equal(st.selected, 1);
  assert.equal(key('d'), 'none');
  assert.equal(key('j'), 'none', 'another key disarms dd'); assert.equal(st.pendingDelete, false);
  assert.deepEqual(st.facts, ['a', 'b']);
  assert.equal(key('s', { ctrl: true }), 'saved-notice');
  assert.equal(key('q'), 'close');
  assert.equal(key('Escape'), 'close');
});

test('card editing: a adds and edits, Enter commits (empty deletes), Esc cancels', () => {
  let st = openCard(['a']);
  let r = cardNavKey(st, { key: 'a' });
  st = r.st;
  assert.deepEqual(st.facts, ['a', '']);
  assert.equal(st.editing, '');
  assert.deepEqual(factsToSave(st), ['a'], 'the empty placeholder is never sent');
  assert.equal(cancelEdit(st).facts.length, 1, 'Esc removes a new empty fact');
  r = commitEdit(st, '  new fact ');
  assert.equal(r.effect, 'save');
  assert.deepEqual(r.st.facts, ['a', 'new fact']);
  assert.equal(r.st.editing, null);
  st = cardNavKey({ ...r.st, selected: 0 }, { key: 'e' }).st;
  assert.equal(st.editing, 'a');
  assert.deepEqual(cancelEdit(st).facts, ['a', 'new fact'], 'Esc keeps an existing fact');
  r = commitEdit(st, '   ');
  assert.deepEqual(r.st.facts, ['new fact'], 'an empty edit deletes the fact');
  assert.equal(r.st.selected, 0);
  const full = openCard(Array.from({ length: MAX_FACTS }, (_, i) => `f${i}`));
  assert.equal(cardNavKey(full, { key: 'a' }).effect, 'full');
  assert.equal(cardNavKey(openCard([]), { key: 'e' }).st.editing, null);
});

test('the project is the open session’s, else the list filter; card titles', () => {
  assert.equal(activeProject('p1', 'p2'), 'p1');
  assert.equal(activeProject(null, 'p2'), 'p2');
  assert.equal(activeProject(undefined, ''), null);
  assert.equal(cardTitle('user', 'x'), 'User');
  assert.equal(cardTitle('project', 'rsi'), 'rsi');
  assert.equal(cardTitle('project', undefined), 'Project');
});

test('the Tauri commands forward one daemon method each and are in the handler list', () => {
  const rs = readFileSync(here('../src-tauri/src/assist.rs'), 'utf8');
  for (const [cmd, method] of [
    ['memory_search', 'MemorySearch'], ['ai_command', 'GenerateText'], ['ai_chat', 'GenerateText'],
    ['query_memory', 'QueryMemory'], ['get_entity_card', 'GetEntityCard'], ['set_entity_card', 'SetEntityCard'],
  ]) {
    const at = rs.indexOf(`pub async fn ${cmd}(`);
    assert.ok(at > 0, cmd);
    assert.ok(rs.indexOf(`"${method}"`, at) > at, `${cmd} forwards ${method}`);
  }
  // No model, provider, endpoint or key is ever sent by the desktop.
  assert.doesNotMatch(rs, /"api_key"|"base_url"|"provider"|"model"\s*:/);
  const handlers = readFileSync(here('../src-tauri/src/handlers.generated.rs'), 'utf8');
  for (const cmd of ['memory_search', 'ai_command', 'ai_chat', 'query_memory', 'get_entity_card', 'set_entity_card']) {
    assert.match(handlers, new RegExp(`assist::${cmd}\\b`));
  }
  assert.match(readFileSync(here('../src-tauri/src/modules.generated.rs'), 'utf8'), /^pub mod assist;$/m);
  // The webview calls them with the argument names the Rust side takes.
  const ts = readFileSync(here('../src/assist.ts'), 'utf8');
  assert.match(ts, /invoke<unknown>\('memory_search', \{ query: q \}\)/);
  assert.match(ts, /invoke<unknown>\('ai_command', \{ instruction, sourceText \}\)/);
  assert.match(ts, /invoke<unknown>\('ai_chat', \{ sourceText, turns \}\)/);
  assert.match(ts, /invoke<unknown>\('query_memory', \{ query, projectId, history: turns \}\)/);
});
