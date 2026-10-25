// Gap sweep G4 (hub #3000): the create-entity form keys, the pickers' Space
// leader, the text-area background editor and the prompts' chords and
// settings side (docs/keybindings.md overlay tables).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { formKey, stepChoice } from '../dist/entity-form-keys.js';
import { hexEditKey, normalizeHex, pickerLeader } from '../dist/overlay-keys.js';
import { parseTextBg, textBgCss } from '../dist/textbg.js';
import {
  appointArgs, effortBars, freshPlan, grantStatus, launchBlock, launchSummary, launchedId, policyLabel, promptKey, regrantsSaved,
  scopeChoices, settingRows, settingsKey, stepEffort, stepPlanPreset, stepPreset, stepScope,
} from '../dist/prompt-model.js';
import { SLICE as VIEWS } from '../dist/actions/views.js';
import { sliceProblems } from '../dist/registry.js';

const ctx = (field, editing = false, pending = '') => ({ field, editing, pending });
const fk = (c, key, more = {}) => formKey(c, { key, ...more }).cmd;

test('create-entity normal mode: field letters, kind h/l, provider, effort, sandbox, pickers, create and close', () => {
  assert.deepEqual(fk(ctx('other'), 'n'), { kind: 'focus', field: 'name' });
  assert.deepEqual(fk(ctx('other'), 'b'), { kind: 'focus', field: 'prompt' });
  assert.deepEqual(fk(ctx('other'), 'k'), { kind: 'focus', field: 'kind' });
  assert.deepEqual(fk(ctx('other'), 'T'), { kind: 'focus', field: 'tags' });
  assert.deepEqual(fk(ctx('other'), 't'), { kind: 'focus', field: 'topology' });
  assert.deepEqual(fk(ctx('kind'), 'l'), { kind: 'kind', by: 1 });
  assert.deepEqual(fk(ctx('kind'), 'h'), { kind: 'kind', by: -1 });
  assert.deepEqual(fk(ctx('other'), 'h'), { kind: 'pass' }, 'h changes the kind only on the kind field');
  assert.deepEqual(fk(ctx('other'), 'p', { ctrlKey: true }), { kind: 'provider', by: 1 });
  assert.deepEqual(fk(ctx('other'), 'P', { ctrlKey: true, shiftKey: true }), { kind: 'provider', by: -1 });
  assert.deepEqual(fk(ctx('other'), 'e'), { kind: 'effort', by: 1 });
  assert.deepEqual(fk(ctx('other'), 'E'), { kind: 'effort', by: -1 });
  assert.deepEqual(fk(ctx('other'), 's'), { kind: 'sandbox' });
  assert.deepEqual(fk(ctx('other'), 'm'), { kind: 'model-picker' });
  const g = formKey(ctx('other'), { key: 'g' });
  assert.deepEqual(g, { cmd: { kind: 'swallow' }, pending: 'g' });
  assert.deepEqual(fk(ctx('other', false, 'g'), 'p'), { kind: 'parent-picker' });
  assert.deepEqual(fk(ctx('kind'), 'Enter'), { kind: 'create' });
  assert.deepEqual(fk(ctx('other'), 'Escape'), { kind: 'close' });
  assert.deepEqual(fk(ctx('other'), 'i'), { kind: 'focus', field: 'name' });
});

test('create-entity insert, body and any-mode keys', () => {
  assert.deepEqual(fk(ctx('name', true), 'n'), { kind: 'pass' }, 'typing goes to the field');
  assert.deepEqual(fk(ctx('name', true), 'Escape'), { kind: 'leave-edit' });
  assert.deepEqual(fk(ctx('prompt', true), 'Escape'), { kind: 'pass' }, 'the body’s Esc is the vim editor’s');
  assert.deepEqual(fk(ctx('name', true), 'Enter', { ctrlKey: true }), { kind: 'create' });
  assert.deepEqual(fk(ctx('tags', true), 'd', { ctrlKey: true }), { kind: 'discard' });
  assert.deepEqual(formKey(ctx('name', true), { key: 'Enter', metaKey: true }, true).cmd, { kind: 'create' }, 'Cmd-Enter on macOS');
});

test('create-entity topology keys', () => {
  assert.deepEqual(fk(ctx('topology'), 'j'), { kind: 'topology-move', by: 1 });
  assert.deepEqual(fk(ctx('topology'), 'ArrowUp'), { kind: 'topology-move', by: -1 });
  assert.deepEqual(fk(ctx('topology'), 'G'), { kind: 'topology-move', by: Infinity });
  assert.deepEqual(fk(ctx('topology'), ' '), { kind: 'topology-preview' });
  assert.deepEqual(fk(ctx('topology'), 'Enter'), { kind: 'topology-choose' });
  assert.deepEqual(fk(ctx('topology'), 'Escape'), { kind: 'topology-clear' });
  assert.equal(stepChoice(['Task', 'Bug'], 'Bug', 1), 'Task');
  assert.equal(stepChoice(['Task', 'Bug'], 'Task', -1), 'Bug');
});

test('the picker leader: Space then N opens a blank prompt, Space then ; the palette', () => {
  let l = pickerLeader(false, { key: ' ' }, true);
  assert.deepEqual(l, { armed: true, run: 'arm' });
  assert.deepEqual(pickerLeader(true, { key: 'N' }, true), { armed: false, run: 'launch' });
  assert.deepEqual(pickerLeader(true, { key: ';' }, true), { armed: false, run: 'palette' });
  assert.deepEqual(pickerLeader(true, { key: 'x' }, true), { armed: false, run: null }, 'another key cancels and acts normally');
  assert.deepEqual(pickerLeader(false, { key: ' ' }, false), { armed: false, run: null }, 'Space types into a non-empty filter');
});

test('the text-area background editor mirrors text_area_bg_editor.rs', () => {
  assert.equal(normalizeHex('a1b2c3'), '#A1B2C3');
  assert.equal(normalizeHex('#a1b2c3'), '#A1B2C3');
  assert.equal(normalizeHex('#abc'), null);
  let st = { input: '', armed: false };
  const press = (key) => { const r = hexEditKey(st.input, st.armed, { key }); st = { input: r.input, armed: r.armed }; return r.effect; };
  for (const c of '#a1b2c') assert.deepEqual(press(c), { kind: 'none' });
  assert.equal(st.input, '#A1B2C', 'typed hex is uppercased');
  assert.deepEqual(press('3'), { kind: 'commit', value: '#A1B2C3' }, 'the seventh character commits');
  assert.deepEqual(press('f'), { kind: 'none' }, 'at most seven characters');
  assert.equal(st.input, '#A1B2C3');
  assert.deepEqual(press('z'), { kind: 'none' });
  press('Backspace');
  assert.equal(st.input, '#A1B2C');
  assert.deepEqual(press('Enter'), { kind: 'commit', value: null }, 'an incomplete colour is an error');
  assert.deepEqual(press('Delete'), { kind: 'clear' });
  assert.equal(st.input, '');
  assert.deepEqual(press('Enter'), { kind: 'commit', value: '' }, 'empty clears the override');
  assert.deepEqual(press('q'), { kind: 'close' });
  assert.deepEqual(press(' '), { kind: 'leader', run: 'arm' });
  assert.deepEqual(press('N'), { kind: 'leader', run: 'launch' });
  assert.deepEqual(parseTextBg('{"on":true,"hex":"#00ff00"}'), { on: true, hex: '#00FF00' });
  assert.deepEqual(parseTextBg('junk'), { on: false, hex: '' });
  assert.equal(textBgCss({ on: false, hex: '#000000' }), '');
  assert.match(textBgCss({ on: true, hex: '' }), /var\(--mantle\)/);
  assert.match(textBgCss({ on: true, hex: '#112233' }), /#112233/);
});

const pc = (kind, over = {}) => ({ kind, normal: false, pending: false, leader: false, submitOnEnter: true, ...over });

test('prompt chords: the launch prompt, the continue prompt and the input modal', () => {
  assert.equal(promptKey(pc('launch'), { key: 'q', ctrlKey: true }), 'close');
  assert.equal(promptKey(pc('continue'), { key: 'q', ctrlKey: true }), 'close');
  assert.equal(promptKey(pc('launch'), { key: 'o', ctrlKey: true }), 'settings');
  assert.equal(promptKey(pc('continue'), { key: 'o', ctrlKey: true }), 'pass', 'only blank prompts flip');
  assert.equal(promptKey(pc('launch'), { key: 'm', ctrlKey: true }), 'model');
  assert.equal(promptKey(pc('launch'), { key: 'e', ctrlKey: true }), 'effort');
  assert.equal(promptKey(pc('launch'), { key: 'b', ctrlKey: true }), 'sandbox');
  assert.equal(promptKey(pc('launch'), { key: 't', ctrlKey: true }), 'launch-tab');
  assert.equal(promptKey(pc('launch'), { key: 's', ctrlKey: true }), 'launch-split');
  assert.equal(promptKey(pc('modal'), { key: 'y', ctrlKey: true }), 'compile');
  assert.equal(promptKey(pc('continue'), { key: 'G', ctrlKey: true, shiftKey: true }), 'grammar');
  assert.equal(promptKey(pc('launch'), { key: 'Tab' }), 'pass', 'Tab in insert mode stays the field’s');
  assert.equal(promptKey(pc('launch', { normal: true }), { key: 'Tab' }), 'settings');
  assert.equal(promptKey(pc('continue', { normal: true }), { key: '?' }), 'help');
  assert.equal(promptKey(pc('continue', { normal: true, pending: true }), { key: '?' }), 'pass', '? completes f? and the like');
  assert.equal(promptKey(pc('continue'), { key: '?' }), 'pass', '? types in insert mode');
  assert.equal(promptKey(pc('launch', { normal: true }), { key: ' ' }), 'arm');
  assert.equal(promptKey(pc('launch', { leader: true }), { key: 'N' }), 'stack');
  assert.equal(promptKey(pc('modal'), { key: 'Enter' }), 'send');
  assert.equal(promptKey(pc('modal', { submitOnEnter: false }), { key: 'Enter' }), 'pass');
  assert.equal(promptKey(pc('modal'), { key: 'Enter', shiftKey: true }), 'pass');
  assert.equal(promptKey(pc('modal', { normal: true }), { key: 'Enter' }), 'pass', 'Vim normal mode: Enter sends nothing (insert mode sends, as the TUI)');
  assert.equal(promptKey(pc('modal', { normal: true }), { key: 'Enter', ctrlKey: true }), 'pass', 'Ctrl-Enter is the field\'s own send chord');
  assert.equal(promptKey(pc('launch'), { key: 'k', metaKey: true }), 'pass', 'Cmd chords stay the platform’s');
});

const settings = (over = {}) => ({ provider: 'Claude', model: 'claude-opus-5', effort: '', sandbox: false, manager: null, ...over });

test('the settings side: rows, keys, effort ladder, scope and policy cycles', () => {
  assert.deepEqual(settingRows(settings()), ['model', 'effort', 'sandbox', 'manager']);
  const planned = settings({ manager: { scope: { kind: 'project' }, preset: 'execute' } });
  assert.deepEqual(settingRows(planned), ['model', 'effort', 'sandbox', 'manager', 'scope', 'policy', 'grant']);
  assert.deepEqual(settingsKey('model', { key: 'l' }), { kind: 'model' });
  assert.deepEqual(settingsKey('effort', { key: 'h' }), { kind: 'effort', by: -1 });
  assert.deepEqual(settingsKey('effort', { key: ' ' }), { kind: 'effort', by: 1 }, 'Space steps forward');
  assert.deepEqual(settingsKey('sandbox', { key: 'Enter' }), { kind: 'sandbox' });
  assert.deepEqual(settingsKey('scope', { key: 'l' }), { kind: 'scope', by: 1 });
  assert.deepEqual(settingsKey('policy', { key: 'h' }), { kind: 'policy', by: -1 });
  assert.deepEqual(settingsKey('grant', { key: 'Enter' }), { kind: 'grant' });
  assert.deepEqual(settingsKey('model', { key: 'p' }), { kind: 'grant' }, 'p toggles the grant from any row');
  assert.deepEqual(settingsKey('model', { key: 'j' }), { kind: 'move', by: 1 });
  assert.deepEqual(settingsKey('model', { key: 'Escape' }), { kind: 'back' });
  assert.equal(stepEffort('claude-opus-5', '', 1), 'low');
  assert.equal(stepEffort('claude-opus-5', 'max', 1), '', 'wraps to the model default');
  assert.equal(stepEffort('claude-opus-4-5', 'high', 1), '', 'Opus 4.5 stops at high');
  assert.equal(effortBars('claude-opus-4-5', 'medium'), '●●○');
  assert.equal(stepPreset('full', 1), 'observe');
  const sessions = [
    { id: 'g2', session_kind: 'Group', project_id: 'P', status: 'Running', title: 'beta' },
    { id: 'g1', session_kind: 'Group', project_id: 'P', status: 'Running', title: 'Alpha' },
    { id: 'e1', session_kind: 'Epic', parent_id: 'g1', project_id: 'P', status: 'Running', title: 'E' },
    { id: 'gx', session_kind: 'Group', project_id: 'P', status: 'Archived', title: 'old' },
    { id: 'go', session_kind: 'Group', project_id: 'Q', status: 'Running', title: 'other' },
  ];
  const choices = scopeChoices(sessions, 'P');
  assert.deepEqual(choices.map((c) => (c.kind === 'project' ? 'project' : c.id)), ['project', 'g1', 'e1', 'g2']);
  assert.deepEqual(stepScope(choices, choices[3], 1), { kind: 'project' });
});

test('the launch header summarizes the launch, and the launched id is read from the answer', () => {
  const s = launchSummary(settings({ effort: 'high', sandbox: true, manager: { scope: { kind: 'project' }, preset: 'full' } }), '/repo');
  assert.equal(s, 'Starts Claude Code (Anthropic CLI) with model claude-opus-5, ●●●○○ high effort. Project: none. Folder: /repo. Own git worktree: yes. Manager: Full project control.');
  assert.equal(launchedId({ id: 'E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE' }), 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae');
  assert.equal(launchedId({ session: { id: 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae' } }), 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae');
  assert.equal(launchedId(null), null);
});

// desktop #204, mirroring the TUI's GrantContextState / block_unloaded_grant /
// appoint_launched_manager (crates/rsi/src/overlay/launch_settings.rs, #3270).
const PID = '11111111-1111-4111-8111-111111111111';
const SID = '22222222-2222-4222-8222-222222222222';
const SAVED = { project_id: PID, row_version: 4, policy: { capabilities: ['observe'] }, revoked: false };

test('#204: an unchanged Policy re-grants the saved grant (presetChosen false, as the TUI)', () => {
  const plan = freshPlan();
  assert.deepEqual(plan, { scope: { kind: 'project' }, preset: 'execute', presetChosen: false });
  const loaded = { state: 'loaded', projectId: PID, saved: SAVED };
  assert.equal(regrantsSaved(plan, loaded), true);
  assert.equal(policyLabel(plan, loaded), 'Saved grant');
  assert.deepEqual(appointArgs(plan, PID, SID), {
    projectId: PID, sessionId: SID, scopeKind: 'project', scopeId: null, preset: 'execute', presetChosen: false,
  });
  assert.ok(launchSummary(settings({ manager: plan }), '/repo', loaded).endsWith('Manager: saved grant.'));
  // A project without a saved grant gets the preset, still unchosen.
  const none = { state: 'loaded', projectId: PID, saved: null };
  assert.equal(regrantsSaved(plan, none), false);
  assert.equal(policyLabel(plan, none), 'Execute');
});

test('#204: a changed Policy sends the chosen preset', () => {
  const loaded = { state: 'loaded', projectId: PID, saved: SAVED };
  const stepped = stepPlanPreset(freshPlan(), 1);
  assert.deepEqual(stepped, { scope: { kind: 'project' }, preset: 'full', presetChosen: true });
  assert.equal(regrantsSaved(stepped, loaded), false);
  assert.equal(policyLabel(stepped, loaded), 'Full project control');
  // Stepping back to the default preset is still a choice (the TUI's step_preset).
  const back = stepPlanPreset(stepped, -1);
  assert.equal(back.preset, 'execute');
  assert.equal(back.presetChosen, true);
  const group = { ...stepped, scope: { kind: 'group', id: SID, label: 'g' } };
  assert.deepEqual(appointArgs(group, PID, SID), {
    projectId: PID, sessionId: SID, scopeKind: 'group', scopeId: SID, preset: 'full', presetChosen: true,
  });
});

test('#204: the launch is blocked until the saved grant loads', () => {
  const plan = freshPlan();
  assert.equal(launchBlock(null, null, PID), null, 'no planned manager, no block');
  assert.equal(launchBlock(plan, null, PID), 'Loading saved grant…');
  assert.equal(launchBlock(plan, { state: 'loading', projectId: PID }, PID), 'Loading saved grant…');
  assert.equal(
    launchBlock(plan, { state: 'failed', projectId: PID, reason: 'grant read timed out' }, PID),
    'Saved grant unavailable: grant read timed out. Press p on the Grant row to retry.',
  );
  assert.equal(launchBlock(plan, { state: 'loaded', projectId: 'other', saved: SAVED }, PID), 'Loading saved grant…', 'a read for another project does not count');
  assert.match(launchBlock(plan, { state: 'loaded', projectId: PID, saved: SAVED }, ''), /^Choose a project first/);
  assert.equal(launchBlock(plan, { state: 'loaded', projectId: PID, saved: SAVED }, PID), null);
  assert.equal(launchBlock(plan, { state: 'loaded', projectId: PID, saved: null }, PID), null, 'no saved grant is a successful read');
  assert.equal(grantStatus({ state: 'loaded', projectId: PID, saved: null }), null);
});

test('#204: the dialog reads the saved grant, gates the Launch button and passes the plan to the appointment', async () => {
  const { readFile } = await import('node:fs/promises');
  const prompts = await readFile(new URL('../src/prompts.ts', import.meta.url), 'utf8');
  assert.match(prompts, /invoke<unknown>\('harness_manager_policy_get', \{ projectId: pid \}\)/);
  assert.match(prompts, /button\.disabled = why !== null/);
  assert.match(prompts, /invoke<\{ message\?: string \}>\('appoint_launched_manager', appointArgs\(plan, launchProject, id\)\)/);
  const launch = await readFile(new URL('../src/launch.ts', import.meta.url), 'utf8');
  assert.ok(launch.indexOf('launchBlocked()') < launch.indexOf("invoke<unknown>('launch_prompt'"), 'submitLaunch checks the block before any launch');
  const rust = await readFile(new URL('../src-tauri/src/prompts.rs', import.meta.url), 'utf8');
  assert.match(rust, /preset_chosen: Option<bool>/);
});

test('the G4 slice registers cleanly', () => {
  assert.equal(VIEWS.actions.length, 1);
  assert.deepEqual(sliceProblems(), []);
});

test('the budget policy form: field cycle, scope kind and tier cycles, digit-only limits, one dot in the ratio', async () => {
  const { budgetFormKey, formFor, policyLine, FIELDS } = await import('../dist/budgets-model.js');
  let st = { form: formFor(null), field: 0 };
  const press = (key, more = {}) => { const r = budgetFormKey(st, { key, ...more }); st = r.st; return r.effect.kind; };
  assert.equal(press('ArrowDown'), 'none');
  assert.equal(st.form.scope_kind, 'provider');
  press('ArrowUp'); press('ArrowUp');
  assert.equal(st.form.scope_kind, 'operator', 'wraps');
  press('x');
  assert.equal(st.form.scope_kind, 'operator', 'typing does nothing on a cycle field');
  press('Tab');
  for (const c of 'p1') press(c);
  assert.equal(st.form.scope_id, 'p1');
  press('Tab', { shiftKey: true });
  assert.equal(st.field, 0);
  st = { ...st, field: FIELDS.indexOf('model_tier') };
  press('ArrowDown');
  assert.equal(st.form.model_tier, 'local');
  st = { ...st, field: FIELDS.indexOf('max_total_tokens') };
  for (const c of '12a3') press(c);
  assert.equal(st.form.max_total_tokens, '123', 'digits only');
  press('Backspace');
  assert.equal(st.form.max_total_tokens, '12');
  st = { ...st, field: FIELDS.indexOf('alert_threshold_ratio') };
  for (const c of '0.8.5') press(c);
  assert.equal(st.form.alert_threshold_ratio, '0.85', 'one dot');
  assert.equal(press('Enter'), 'save');
  assert.equal(press('Escape'), 'close');
  assert.equal(press('Tab'), 'none');
  const edited = formFor({ scope_kind: 'global', scope_id: 'global', model_tier: 'premium', max_concurrency: 2 });
  assert.equal(edited.scope_id, '', 'Global keeps no id');
  assert.equal(edited.max_concurrency, '2');
  assert.equal(policyLine({ scope_kind: 'project', scope_id: 'p', model_tier: 'premium', max_total_tokens: 5, alert_threshold_ratio: 0.8 }), 'project/p · premium · 5 tokens, alert at 80%');
});

test('the settings pane: a / d list keys, provider-key keys and panel widths', async () => {
  const { paneKey, resizePanels, panelColumns, parsePanels } = await import('../dist/settings-model.js');
  const nav = { group: 0, focus: 'items', sectionOf: {}, rowOf: {} };
  assert.deepEqual(paneKey(nav, 'a', false, 3), { kind: 'item', op: 'add' });
  assert.deepEqual(paneKey(nav, 'd', false, 3), { kind: 'item', op: 'delete' });
  assert.deepEqual(paneKey(nav, 'c', false, 3), { kind: 'provider-key', op: 'c' });
  assert.deepEqual(paneKey(nav, '>', false, 3), { kind: 'resize', op: '>' });
  assert.equal(paneKey(nav, 'K', false, 3).kind, 'missing');
  let w = { rail: null, info: null };
  w = resizePanels(w, 'rail', '>');
  assert.equal(w.rail, 254);
  w = resizePanels(w, 'items', '>');
  assert.equal(w.info, 276, 'the list takes room from the info card');
  assert.equal(panelColumns(w), '254px minmax(0, 1fr) 276px');
  assert.deepEqual(resizePanels(w, 'items', '='), { rail: null, info: null });
  assert.equal(panelColumns({ rail: null, info: null }), '');
  assert.deepEqual(parsePanels('{"rail":200}'), { rail: 200, info: null });
});

test('the DAG inspector readbacks: ids from rows, the preview head and body, bucket lines', async () => {
  const { artifactRef, bucketLines, liveAttemptOf, previewBody, previewHead, validationOf } = await import('../dist/dag-readbacks.js');
  const G = 'e2cae1d4-1833-48eb-a7e8-6f1dd480fdae';
  assert.deepEqual(artifactRef({ artifact_id: 7, graph_id: G }, null), { graphId: G, artifactId: 7 });
  assert.deepEqual(artifactRef({ id: '9' }, G), { graphId: G, artifactId: 9 }, 'falls back to the hydrated graph');
  assert.equal(artifactRef({ id: 'x' }, G), null);
  assert.equal(validationOf({ validation_id: G }), G);
  assert.equal(liveAttemptOf({ summary: { live_attempt_id: G } }), G);
  assert.equal(liveAttemptOf({ status: 'x' }), null);
  const p = { content_state: 'complete', content_type: 'text/plain', shown_lines: 80, total_lines: 200, shown_bytes: 10, total_bytes: 99, truncated: true, truncated_by_lines: true, truncated_by_bytes: false, applied_max_lines: 80, applied_max_bytes: 16384, text: 'hello' };
  assert.equal(previewHead(p), 'complete · text/plain · 80 of 200 lines · 10 of 99 bytes · truncated by lines (max 80 lines, 16384 bytes)');
  assert.equal(previewBody(p), 'hello');
  assert.equal(previewBody({ binary_unavailable_reason: 'binary' }), 'binary or unavailable: binary');
  assert.deepEqual(bucketLines({ live_attempt_id: G, prompt: { a: 1 }, diff: null, tests: [1, 2] }), ['prompt: 1 field', 'diff: none', 'tests: 2 items']);
});

test('the New session summary uses only glyphs every UI font has (local #283: ▯ and ⊡ drew as boxes)', () => {
  const settings = { provider: 'Claude', model: 'claude-opus-5', effort: 'high', sandbox: true, manager: { preset: 'full' } };
  const text = [launchSummary(settings, '/repo'), launchSummary({ ...settings, sandbox: false, effort: '' }, '')].join(' ')
    + effortBars('claude-opus-5', 'low') + effortBars('claude-opus-5', '');
  const odd = [...new Set([...text].filter((c) => c.charCodeAt(0) > 127))].filter((c) => !'·●○★’'.includes(c)); // ’ is the apostrophe in "the daemon’s default" (#289)
  assert.deepEqual(odd, []);
  assert.match(text, /●/);
  assert.match(text, /○/);
});
