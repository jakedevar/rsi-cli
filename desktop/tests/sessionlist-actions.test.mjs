import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ACTIONS, appMenuModel, menuItems, contextMenuModel, keyHints, paletteEntries, unavailable } from '../dist/registry.js';
import { COMMAND_BINDINGS, KEY_BINDINGS } from '../dist/keybindings.js';
import { SESSIONLIST_ACTIONS, SESSIONLIST_COMMANDS, SESSIONLIST_KEYS } from '../dist/actions/sessionlist.js';
import { desktopKeymap } from '../dist/keybindings.js';
import { KeyEngine } from '../dist/keyengine.js';

const list = (over = {}) => ({
  cursorFoldable: false, cursorHasParent: false, foldable: 0, paneDetail: false, attentionQueue: 0,
  search: '', detailSearchable: false, projects: 0, labelGroups: false, openHasParent: false, ...over,
});
const snap = (over = {}, l = {}) => ({
  open: { status: 'Completed', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false, event: null, list: list(l), ...over,
});
const none = () => snap({ open: null });

test('every slice-4 action is registered once and every key and command it names exists', () => {
  const ids = new Set(ACTIONS.map((a) => a.id));
  for (const a of SESSIONLIST_ACTIONS) assert.ok(ids.has(a.id), a.id);
  for (const [k, b] of Object.entries(SESSIONLIST_KEYS)) {
    assert.equal(KEY_BINDINGS[k].action, b.action, k);
    assert.ok(ids.has(b.action), k);
  }
  for (const [c, b] of Object.entries(SESSIONLIST_COMMANDS)) {
    assert.equal(COMMAND_BINDINGS[c].action, b.action, c);
    assert.ok(ids.has(b.action), c);
  }
});

test('the TUI keys of this slice reach their actions through the engine', () => {
  const press = (keys) => {
    const e = new KeyEngine(desktopKeymap());
    let o;
    for (const k of keys) o = e.feed(k);
    return o;
  };
  const cases = [
    [['Space', 'p'], 'choose-project'], [['Space', 's'], 'choose-sort'], [['Space', 'C'], 'change-session-project'],
    [['F3'], 'session-info'], [[']', 'g'], 'label-next'], [['[', 'g'], 'label-prev'],
    [['z', 'o'], 'fold-open'], [['z', 'c'], 'fold-close'], [['z', 'a'], 'fold-toggle'],
    [['z', 'M'], 'fold-close-all'], [['z', 'R'], 'fold-open-all'], [['-'], 'ascend'], [['H'], 'ascend'],
    [['m', 'p'], 'move-to-parent'], [['m', 'o'], 'move-to-root'], [['U'], 'unarchive-session'],
    [['Space', 'n'], 'open-notifications'], [['n'], 'search-next'], [['N'], 'search-prev'],
  ];
  for (const [keys, action] of cases) assert.equal(press(keys).action, action, keys.join(' '));
  const slot = press(['Space', '7']);
  assert.equal(slot.action, 'attention-slot');
  assert.equal(slot.arg, '7');
});

test('the : commands of this slice run their actions, with the argument', () => {
  const e = new KeyEngine(desktopKeymap());
  assert.deepEqual(e.runCommand('projects'), { kind: 'run', action: 'choose-project', count: null, keys: ':projects' });
  const sw = e.runCommand('project api');
  assert.equal(sw.action, 'choose-project');
  assert.equal(sw.arg, 'api');
  assert.equal(e.runCommand('project-new web /srv/web').arg, 'web /srv/web');
  assert.equal(e.runCommand('project-edit').action, 'project-edit');
  assert.equal(e.runCommand('project-delete api').arg, 'api');
  assert.match(e.runCommand('project-delete').message, /needs an argument/);
  assert.equal(e.runCommand('groups').action, 'choose-label', 'the alias resolves to :group');
  // <Space>n and :alerts are slice 6's notification overlay; the numbered queue stays in the palette and Go menu.
  for (const alias of ['alerts', 'att', 'attention']) assert.equal(e.runCommand(alias).action, 'open-notifications', alias);
});

test('session actions need an open session; label, project and parent moves need it writable', () => {
  for (const id of ['session-info', 'choose-label', 'change-session-project', 'move-to-parent', 'move-to-root']) {
    assert.match(unavailable(id, none()), /Open a session first/, id);
  }
  assert.equal(unavailable('session-info', snap()), null);
  const ro = snap({ open: { status: 'Archived', readOnly: true, pauseLevel: 'none', hasQuestion: false }, view: 'archived' });
  assert.equal(unavailable('session-info', ro), null, 'archived sessions still show their info');
  for (const id of ['choose-label', 'change-session-project', 'move-to-parent']) assert.match(unavailable(id, ro), /read-only/, id);
  assert.match(unavailable('move-to-root', snap()), /already at the top/);
  assert.equal(unavailable('move-to-root', snap({}, { openHasParent: true })), null);
});

test('folds, hierarchy, labels and attention say why they cannot run', () => {
  for (const id of ['fold-open', 'fold-close', 'fold-toggle']) {
    assert.match(unavailable(id, snap()), /children/, id);
    assert.equal(unavailable(id, snap({}, { cursorFoldable: true })), null, id);
    // With the transcript pane active the fold acts on slice 5's selected event (actions/folds.ts).
    assert.match(unavailable(id, snap({}, { paneDetail: true })), /Select an event first/, id);
    assert.equal(unavailable(id, snap({ detail: { events: 2, shown: 2, userMessages: 1, cursor: true, focus: 'transcript', hideSystem: false, hideThinking: false } })), null, `${id} folds the selected event`);
  }
  for (const id of ['fold-open-all', 'fold-close-all']) {
    assert.match(unavailable(id, snap()), /No session has children/, id);
    assert.equal(unavailable(id, snap({}, { foldable: 2 })), null, id);
  }
  assert.match(unavailable('ascend', snap()), /no parent/);
  assert.equal(unavailable('ascend', snap({}, { cursorHasParent: true })), null);
  assert.match(unavailable('label-next', snap()), /different label/);
  assert.equal(unavailable('label-prev', snap({}, { labelGroups: true })), null);
  assert.match(unavailable('attention-slot', snap()), /waiting for you/);
  assert.equal(unavailable('attention-slot', snap({}, { attentionQueue: 2 })), null);
  assert.equal(unavailable('attention-list', none()), null, 'an empty queue still opens, saying so');
});

test('search actions and the project and sort pickers', () => {
  assert.match(unavailable('search-next', snap()), /Type a filter/);
  assert.equal(unavailable('search-next', snap({}, { detailSearchable: true, search: 'x' })), null);
  assert.equal(unavailable('search-next', snap({}, { search: 'x' })), null, 'G2: the filtered list rows are the matches with no session open');
  assert.match(unavailable('clear-search', snap()), /No filter/);
  assert.equal(unavailable('clear-search', snap({}, { search: 'bug' })), null);
  assert.equal(unavailable('choose-project', none()), null);
  assert.equal(unavailable('project-new', none()), null);
  assert.match(unavailable('project-edit', none()), /no projects/);
  assert.match(unavailable('project-delete', snap()), /no projects/);
  assert.equal(unavailable('project-delete', snap({}, { projects: 1 })), null);
  assert.equal(unavailable('choose-sort', none()), null);
  assert.equal(unavailable('choose-project', snap({ modalOpen: true })) !== null, true, 'pickers do not stack over a dialog');
});

test('the palette lists every slice-4 action with its keys', () => {
  const entries = paletteEntries(none(), 'other');
  for (const a of SESSIONLIST_ACTIONS) {
    const e = entries.find((x) => x.id === a.id);
    assert.ok(e, a.id);
    assert.deepEqual(e.keys, keyHints(a.id), a.id);
  }
  assert.deepEqual(keyHints('attention-slot'), Array.from({ length: 9 }, (_, i) => `<Space>${i + 1}`));
  assert.deepEqual(keyHints('choose-project'), ['<Space>p', ':projects', ':project']);
  assert.ok(keyHints('clear-search').includes('Esc'));
});

test('the menu bar and the session-row menu carry the slice-4 actions', () => {
  const bar = appMenuModel('other', snap());
  const ids = bar.flatMap((m) => menuItems(m.items).map((n) => n.id));
  for (const a of SESSIONLIST_ACTIONS) assert.ok(ids.includes(`act:${a.id}`), a.id);
  const view = bar.find((m) => m.id === 'view');
  assert.ok(menuItems(view.items).some((n) => n.id === 'act:choose-sort'));
  const file = bar.find((m) => m.id === 'file').items.filter((n) => n.kind === 'item');
  assert.equal(file[file.length - 1].id, 'act:quit', 'Quit stays last in File');
  const row = contextMenuModel('session-row', snap()).filter((n) => n.kind === 'item').map((n) => n.id);
  for (const id of ['session-info', 'choose-label', 'change-session-project', 'move-to-parent', 'move-to-root', 'unarchive-session']) {
    assert.ok(row.includes(`ctx:${id}`), id);
  }
});

test('desktop #210: the project actions sit in the File menu right after New session…, Quit stays last', () => {
  const bar = appMenuModel('other', snap());
  const file = bar.find((m) => m.id === 'file').items.filter((n) => n.kind === 'item').map((n) => n.id);
  const view = menuItems(bar.find((m) => m.id === 'view').items).map((n) => n.id);
  assert.deepEqual(file.slice(0, 4), ['act:launch', 'act:project-new', 'act:project-edit', 'act:project-delete']);
  assert.equal(file[file.length - 1], 'act:quit');
  for (const id of ['project-new', 'project-edit', 'project-delete']) assert.ok(!view.includes(`act:${id}`), `${id} left the View menu`);
  const titles = bar.find((m) => m.id === 'file').items.filter((n) => n.kind === 'item').map((n) => n.text);
  assert.ok(titles.includes('New project…'), 'the entry reads New project…');
  const mac = appMenuModel('mac', snap()).find((m) => m.id === 'file').items.filter((n) => n.kind === 'item').map((n) => n.id);
  assert.equal(mac[1], 'act:project-new', 'the same order on macOS');
  assert.equal(unavailable('project-new', none()), null, 'New project… works with no projects and no session');
});

test('desktop #210: the keys and :project commands still reach the project actions', () => {
  const e = new KeyEngine(desktopKeymap());
  assert.equal(e.runCommand('project-new').action, 'project-new');
  assert.equal(e.runCommand('project-edit').action, 'project-edit');
  assert.equal(e.runCommand('project-delete web').action, 'project-delete');
  assert.equal(e.runCommand('project').action, 'choose-project');
  assert.equal(SESSIONLIST_KEYS['<Space>p'].action, 'choose-project');
});
