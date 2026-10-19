// Gap sweep G2 (hub #3000): hierarchy creation chords, the create-entity form
// and gL (set Epic lead).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { ACTIONS, keyHints, unavailable } from '../dist/registry.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';
import { CREATE_KIND, HIERARCHY_ACTIONS, HIERARCHY_RULES } from '../dist/actions/hierarchy.js';
import {
  createBlock, creationWorkingDir, draftFor, emptyDraft, entityRequest, leadBlock, legalChildren, normalizeTag,
  parentChoices, parseTags, scopeParent, visibleFields,
} from '../dist/hierarchy-model.js';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));

const group = { id: 'g', session_kind: 'Group', title: 'Web', status: 'Completed', working_dir: '/w' };
const epic = { id: 'e', session_kind: 'Epic', title: 'Login', parent_id: 'g', status: 'Completed', project_id: 'p', working_dir: '/w/e', lead_session_id: null };
const task = { id: 't', session_kind: 'Task', title: 'Fix', parent_id: 'e', status: 'Running', working_dir: '/w/t' };
const loose = { id: 'l', session_kind: 'Standard', title: 'Chat', status: 'Running', working_dir: '/w/l' };
const all = [group, epic, task, loose];
const byId = (id) => all.find((s) => s.id === id);

const snap = (hier, over = {}) => ({
  open: null, cursorOnRow: true, rowCount: 4, attentionRows: 0, view: 'active', vimKeys: true,
  hideTools: false, showFinished: false, modalOpen: false, event: null, hier, ...over,
});
const at = (row) => snap({ row, scope: scopeParent(row, byId), parent: row?.parent_id ? byId(row.parent_id) : null });

test('every hierarchy action is registered with its TUI chord', () => {
  const ids = HIERARCHY_ACTIONS.map((a) => a.id);
  assert.deepEqual(Object.keys(HIERARCHY_RULES).sort(), [...ids].sort());
  for (const id of ids) assert.ok(ACTIONS.some((a) => a.id === id), id);
  assert.deepEqual(keyHints('create-group'), ['<Space>G']);
  assert.deepEqual(keyHints('create-epic'), ['<Space>E']);
  assert.deepEqual(keyHints('create-story'), ['<Space>gS']);
  assert.deepEqual(keyHints('create-task'), ['<Space>gT']);
  assert.deepEqual(keyHints('create-bug'), ['<Space>B']);
  assert.deepEqual(keyHints('set-epic-lead'), ['gL', ':lead']);
  assert.deepEqual(Object.values(CREATE_KIND), ['Group', 'Epic', 'Story', 'Task', 'Bug']);
  const e = new KeyEngine(desktopKeymap());
  e.feed('Space'); e.feed('g');
  assert.equal(e.feed('S').action, 'create-story');
  assert.equal(new KeyEngine(desktopKeymap()).runCommand('setlead').action, 'set-epic-lead');
});

test('the containment matrix is legal_children', () => {
  assert.deepEqual(legalChildren(null), ['Standard', 'Group']);
  assert.deepEqual(legalChildren('Group'), ['Standard', 'Epic']);
  assert.deepEqual(legalChildren('Epic'), ['Story', 'Task', 'Bug', 'Feature', 'Refactor', 'Research']);
  assert.deepEqual(legalChildren('Task'), []);
});

test('a chord creates in the cursor container, else its parent container, else the top level', () => {
  assert.equal(scopeParent(group, byId), group);
  assert.equal(scopeParent(task, byId), epic);
  assert.equal(scopeParent(loose, byId), null);
  assert.equal(scopeParent(null, byId), null);
  // The rules are the chord's precheck toast.
  assert.equal(unavailable('create-group', at(loose)), null);
  assert.match(unavailable('create-group', at(task)), /Group not allowed under Epic “Login”/);
  assert.equal(unavailable('create-epic', at(group)), null);
  assert.match(unavailable('create-epic', at(loose)), /Epic not allowed under the top level \(put the cursor on a Group\)/);
  for (const id of ['create-story', 'create-task', 'create-bug']) {
    assert.equal(unavailable(id, at(task)), null, id);
    assert.equal(unavailable(id, at(epic)), null, id);
    assert.match(unavailable(id, at(group)), /not allowed under Group/, id);
  }
  assert.equal(createBlock('Standard', null), null);
});

test('gL needs a leaf under an Epic', () => {
  assert.equal(unavailable('set-epic-lead', at(task)), null);
  assert.equal(unavailable('set-epic-lead', at(loose)), 'Session has no parent Epic');
  assert.match(unavailable('set-epic-lead', at(epic)), /cannot lead/);
  assert.equal(leadBlock({ id: 'x', session_kind: 'Standard', parent_id: 'g' }, group), 'Parent is not an Epic');
  assert.equal(leadBlock(null, undefined), 'No session selected');
});

test('tags follow normalize_tag', () => {
  assert.deepEqual(normalizeTag('  Foo  Bar '), { ok: true, tag: 'foo-bar' });
  assert.equal(normalizeTag('-x').ok, false);
  assert.deepEqual(parseTags('Web, ui/desktop web'), { tags: ['web', 'ui/desktop'], bad: [] });
  assert.deepEqual(parseTags('ok a.b'), { tags: ['ok'], bad: ['a.b'] });
});

test('fields follow the kind, and the parent choices are the legal containers', () => {
  assert.deepEqual(visibleFields('Group'), ['kind', 'name', 'tags', 'parent']);
  assert.deepEqual(visibleFields('Epic'), ['kind', 'name', 'tags', 'parent', 'topology']);
  assert.ok(visibleFields('Bug').includes('sandbox') && visibleFields('Bug').includes('prompt'));
  assert.deepEqual(parentChoices('Epic', all).map((c) => c.id), ['g']);
  assert.deepEqual(parentChoices('Group', all).map((c) => c.id), [null]);
  assert.deepEqual(parentChoices('Standard', all).map((c) => c.id), [null, 'g']);
  assert.deepEqual(parentChoices('Task', all).map((c) => c.id), ['e']);
  const d = draftFor('Epic', 'g', { tags: 'web', topologyId: 'topo', provider: 'Codex', sandbox: true }, 'Claude');
  assert.equal(d.tags, 'web');
  assert.equal(d.topologyId, 'topo');
  assert.equal(d.provider, 'Codex');
  assert.equal(draftFor('Group', null, { topologyId: 'topo' }, 'Claude').topologyId, null);
});

test('the submit request is CreateContainer for containers and LaunchSession for leaves', () => {
  const ctx = { parentKind: 'Group', projectId: 'p', workingDir: '/w' };
  const epicDraft = { ...emptyDraft('Epic', 'g'), name: ' Login ', tags: 'auth', topologyId: 'topo' };
  assert.deepEqual(entityRequest(epicDraft, ctx), {
    ok: true,
    request: { command: 'create_container', args: { kind: 'Epic', name: 'Login', parentId: 'g', projectId: 'p', tags: ['auth'], topologyId: 'topo' } },
  });
  const groupDraft = { ...emptyDraft('Group', null), name: 'Web', tags: 'web', topologyId: 'topo' };
  assert.equal(entityRequest(groupDraft, { ...ctx, parentKind: null }).request.args.topologyId, null);
  const taskDraft = { ...emptyDraft('Task', 'e', 'Codex'), name: 'Fix', tags: 'bug', prompt: 'fix it', effort: 'ultra', sandbox: true, model: ' ' };
  assert.deepEqual(entityRequest(taskDraft, { parentKind: 'Epic', projectId: 'p', workingDir: '/w/e' }), {
    ok: true,
    request: {
      command: 'launch_entity',
      args: {
        request: {
          kind: 'Task', title: 'Fix', query: 'fix it', provider: 'Codex', model: null, effort: 'ultra',
          parent_id: 'e', project_id: 'p', working_dir: '/w/e', sandbox: true, tags: ['bug'],
        },
      },
    },
  });
});

test('the form refuses what the TUI refuses', () => {
  const ctx = { parentKind: 'Epic', projectId: null, workingDir: '/w' };
  const base = { ...emptyDraft('Task', 'e'), name: 'n', tags: 't', prompt: 'p' };
  assert.equal(entityRequest({ ...base, name: ' ' }, ctx).why, 'Name required');
  assert.equal(entityRequest({ ...base, tags: '' }, ctx).why, 'at least one tag required');
  assert.equal(entityRequest({ ...base, tags: 'a.b' }, ctx).why, 'fix invalid tags before submitting');
  assert.equal(entityRequest(base, { ...ctx, parentKind: 'Group' }).why, 'Task not allowed under Group');
  assert.match(entityRequest({ ...base, prompt: '' }, ctx).why, /prompt/);
  assert.match(entityRequest(base, { ...ctx, workingDir: null }).why, /working directory/);
  assert.equal(creationWorkingDir('/proj', epic, task), '/proj');
  assert.equal(creationWorkingDir(null, epic, task), '/w/e');
  assert.equal(creationWorkingDir(' ', null, task), '/w/t');
  assert.equal(creationWorkingDir(null, null, null), null);
});

test('the run side sends the commands the Rust module defines', () => {
  const run = readFileSync(here('../src/actions/hierarchy-run.ts'), 'utf8');
  const rust = readFileSync(here('../src-tauri/src/hierarchy.rs'), 'utf8');
  for (const cmd of ['create_container', 'launch_entity', 'set_epic_lead']) assert.match(rust, new RegExp(`pub async fn ${cmd}\\(`), cmd);
  assert.match(run, /'set_epic_lead', \{ epicId: p\.id, newLeadSessionId: t\.id \}/);
  assert.match(rust, /"CreateContainer"/);
  assert.match(rust, /"LaunchSession"/);
  assert.match(rust, /"SetEpicLead"/);
});
