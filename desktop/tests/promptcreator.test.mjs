// The prompt creator and the Claude skills list (local #88; hub #3040 / #3062):
// registry entries, <Space>gP, the prompt creator's rules and the DOM code's
// data custody (the request shapes are pinned by the Rust tests in
// src-tauri/src/prompt_files.rs). Hooks, providers and bridges are tested in
// tests/operatorconfig.test.mjs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, paletteEntries } from '../dist/registry.js';
import { PROMPT_CREATOR_ACTIONS, PROMPT_CREATOR_KEY_BINDINGS, PROMPT_CREATOR_RULES } from '../dist/actions/promptcreator.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import {
  NEW_PROMPT_CONTENT, formatSize, movePromptCursor, newPromptName, promptCursorAfter, promptListKey, promptNameError,
  relativeTime,
} from '../dist/prompt-creator-model.js';
import { cursorAfterDelete, skillPreviewScroll, skillRowValue, skillToggledNotice } from '../dist/skills-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const snap = (over = {}) => ({
  open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, ...over,
});

// ---------- registry ----------

test('every slice action is registered with one rule and is in the palette', () => {
  const ids = PROMPT_CREATOR_ACTIONS.map((a) => a.id);
  assert.deepEqual(Object.keys(PROMPT_CREATOR_RULES).sort(), [...ids].sort());
  const entries = paletteEntries(snap(), 'other');
  for (const id of ids) {
    assert.ok(ACTIONS.some((a) => a.id === id), id);
    assert.equal(PROMPT_CREATOR_RULES[id](snap()), null, id);
    assert.ok(entries.some((e) => e.id === id && e.enabled), `${id} in the palette`);
  }
});

test('<Space>gP opens the prompt creator through the TUI keymap', () => {
  const e = new KeyEngine(desktopKeymap());
  let out;
  for (const k of ['Space', 'g', 'P']) out = e.feed(k);
  assert.equal(out.action, 'prompt-creator');
  for (const b of Object.values(PROMPT_CREATOR_KEY_BINDINGS)) assert.ok(PROMPT_CREATOR_ACTIONS.some((a) => a.id === b.action), b.action);
});

// ---------- the prompt creator ----------

test('a new prompt name is a valid flat .md name; bad names are refused like the daemon', () => {
  const name = newPromptName(1_792_318_500_000);
  assert.equal(name, 'prompt-20261018-101500.md');
  assert.equal(promptNameError(name), null);
  for (const bad of ['', '.md', '.x.md', '../x.md', 'a..b.md', 'd/x.md', 'd\\x.md', 'C:x.md', ' x.md', 'x.txt', 'x\u0007.md', `${'a'.repeat(198)}.md`]) {
    assert.notEqual(promptNameError(bad), null, JSON.stringify(bad));
  }
  assert.equal(NEW_PROMPT_CONTENT, '# New Prompt\n\n');
});

test('prompt list keys, cursor and display follow prompt_creator_keys.rs', () => {
  assert.deepEqual(['j', 'ArrowDown', 'k', 'ArrowUp', 'Enter', 'd', 'm', 'q', 'Escape', 'n', 'x'].map((k) => promptListKey(k, false)),
    ['down', 'down', 'up', 'up', 'open', 'delete', 'model', 'close', 'close', 'new', null]);
  assert.equal(promptListKey('n', true), 'new');
  assert.equal(promptListKey('j', true), null);
  assert.equal(movePromptCursor(0, 3, -1), 0);
  assert.equal(movePromptCursor(2, 3, 1), 2);
  assert.equal(movePromptCursor(0, 0, 1), 0);
  const files = [{ name: 'a.md' }, { name: 'b.md' }];
  assert.equal(promptCursorAfter(files, 'b.md', 0), 1);
  assert.equal(promptCursorAfter(files, 'gone.md', 5), 1);
  assert.equal(promptCursorAfter([], null, 3), 0);
  const now = Date.parse('2026-10-18T12:00:00Z');
  assert.deepEqual(['2026-10-18T11:59:30Z', '2026-10-18T11:30:00Z', '2026-10-18T09:00:00Z', '2026-10-15T12:00:00Z', null]
    .map((t) => relativeTime(t, now)), ['just now', '30m ago', '3h ago', '3d ago', '']);
  assert.deepEqual([12, 2048, 3 * 1024 * 1024].map(formatSize), ['12B', '2.0KB', '3.0MB']);
});

// ---------- data custody in the DOM code ----------

test('the slice reaches data only through its Tauri commands', () => {
  for (const f of ['../src/skills.ts', '../src/prompt-creator.ts']) {
    const src = readFileSync(here(f), 'utf8');
    assert.ok(!/\bfetch\(|XMLHttpRequest|\.rsi\/prompts\/[^ ]*\.md['"]/.test(src), f);
    assert.ok(!/innerHTML/.test(src), `${f}: textContent only`);
  }
  const rs = readFileSync(here('../src-tauri/src/prompt_files.rs'), 'utf8');
  assert.ok(!/std::fs|tokio::fs/.test(rs), 'the Tauri side reads no local file');
  for (const m of ['ListPromptFiles', 'GetPromptFile', 'PutPromptFile', 'DeletePromptFile']) {
    assert.ok(rs.includes(`"${m}"`), m);
  }
});

// hub #3062: Settings › Claude Skills and the Skill Preview over ListSkills / GetSkill.
test('a skill row shows its state and description, as the TUI row does', () => {
  const s = { name: 'debug', enabled: true, description: 'Debug it', has_skill_md: true };
  assert.equal(skillRowValue(s), 'enabled · Debug it');
  assert.equal(skillRowValue({ ...s, enabled: false, description: null }), 'disabled');
  assert.equal(skillRowValue({ ...s, has_skill_md: false, description: null }), 'enabled · no SKILL.md');
});

test('the Skill Preview scrolls with the TUI keys and stops at the last line', () => {
  assert.equal(skillPreviewScroll(0, 30, 'j', false), 1);
  assert.equal(skillPreviewScroll(29, 30, 'j', false), 29);
  assert.equal(skillPreviewScroll(0, 30, 'k', false), 0);
  assert.equal(skillPreviewScroll(5, 30, 'd', true), 15);
  assert.equal(skillPreviewScroll(25, 30, 'd', true), 29);
  assert.equal(skillPreviewScroll(5, 30, 'u', true), 0);
  assert.equal(skillPreviewScroll(3, 30, 'G', false), 29);
  assert.equal(skillPreviewScroll(3, 30, 'g', false), 0);
  assert.equal(skillPreviewScroll(3, 30, 'x', false), null);
  assert.equal(skillPreviewScroll(3, 30, 'j', true), null);
});

test('skills are read through rsid only: the overlay calls list_skills and get_skill', () => {
  const ts = readFileSync(here('../src/skills.ts'), 'utf8');
  assert.match(ts, /invoke<SkillsView>\('list_skills'\)/);
  assert.match(ts, /invoke<SkillContent>\('get_skill', \{ name \}\)/);
  const rs = readFileSync(here('../src-tauri/src/input.rs'), 'utf8');
  assert.match(rs, /forward\(&state, METHOD_LIST_SKILLS, list_skills_params\(\)\)/);
  assert.match(rs, /forward\(&state, METHOD_GET_SKILL, get_skill_params\(&name\)\?\)/);
});

test('skills e/d go through rsid (#97): SetSkillEnabled flips the selected skill, DeleteSkill runs on the second d', () => {
  const ts = readFileSync(here('../src/skills.ts'), 'utf8');
  assert.match(ts, /invoke<SkillToggled>\('set_skill_enabled', \{ name: r\.name, enabled: !r\.enabled \}\)/);
  assert.match(ts, /const step = deleteChord\(armed, rows\[skillsAt\]\?\.name\);/);
  assert.match(ts, /if \(step\.run === null \|\| busy\) return;/);
  assert.match(ts, /invoke<SkillDeleted>\('delete_skill', \{ name: step\.run \}\)/);
  assert.match(ts, /Press d again to delete skill/);
  const rs = readFileSync(here('../src-tauri/src/input.rs'), 'utf8');
  assert.match(rs, /METHOD_SET_SKILL_ENABLED: &str = "SetSkillEnabled"/);
  assert.match(rs, /METHOD_DELETE_SKILL: &str = "DeleteSkill"/);
  assert.match(rs, /forward\(&state, METHOD_DELETE_SKILL, delete_skill_params\(&name\)\?\)/);
  const run = readFileSync(here('../src/actions/promptcreator-run.ts'), 'utf8');
  assert.match(run, /keys: SKILL_ROW_KEYS,\s+run: \(\) => void openSkills\(\),/);
});

test('skills e/d notices and cursor follow the TUI', () => {
  assert.equal(skillToggledNotice({ name: 'debug', enabled: false, changed: true, path: '/s/debug.disabled' }), 'Disabled skill: debug');
  assert.equal(skillToggledNotice({ name: 'debug', enabled: true, changed: true, path: '/s/debug' }), 'Enabled skill: debug');
  assert.equal(cursorAfterDelete(2, 2), 1, 'the last row deleted: the cursor moves up');
  assert.equal(cursorAfterDelete(0, 3), 0);
  assert.equal(cursorAfterDelete(0, 0), 0, 'an empty list keeps row 0');
});
