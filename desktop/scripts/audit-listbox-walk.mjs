// The keyboard-list screen-reader walk (local #318). Run: node scripts/audit.mjs flow scripts/audit-listbox-walk.mjs --out DIR
//
// For every view that has a keyboard list, it opens the view in the built app (fake daemon, writes stubbed), presses a move key and
// reads the accessibility wiring: the focused element (or the dialog / panel that holds it) must carry an aria-activedescendant that
// names a visible role=option / treeitem with aria-selected="true" inside a named listbox / tree, and the id must change when the
// move key moves the highlight. A list with one row or none cannot move; the walk says so and checks the pointer only.
//
// The pure verdict is exported for the tests; `walkLists` needs the page. A browser cannot run VoiceOver: this reads the same
// attributes the accessibility tree is built from (WAI-ARIA 1.2 listbox / combobox / tree patterns).
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';

/** Palette action ids that open a keyboard list (titles come from the built registry, so a rename cannot go stale). */
export const LIST_VIEWS = [
  'choose-project', 'choose-sort', 'choose-label', 'issues-open', 'theme-picker', 'theme-roles', 'model-dropdown', 'switch-model',
  'todo-toggle', 'memory-search', 'files-explorer', 'files-find', 'prompt-creator', 'open-trash', 'open-notifications', 'open-recent',
  'admin-schedules', 'admin-remote', 'admin-satellites', 'admin-mcp', 'admin-provider-keys', 'admin-scratch', 'admin-settlement',
  'settings-skills', 'card',
  // local #330: the manager lists, dashboard, DAG, graph review, budgets and settings rail
  'mgr-global', 'mgr-board', 'mgr-dashboard', 'mgr-budget', 'dag-open', 'graph-open', 'settings-open',
];

/**
 * Walk entries that open a view an action opens and then press keys or click, to reach a second list in it (local #334).
 * `settings-rows`: Settings with the keys moved from the group rail to the rows (ArrowRight). `launch-settings`: the launch
 * dialog's More options panel. Both are grids of rows whose cells hold controls, so the walk also checks the cells and controls.
 */
export const EXTRA_VIEWS = {
  'settings-rows': { action: 'settings-open', pre: [{ key: 'ArrowRight' }] },
  'launch-settings': { action: 'launch', pre: [{ click: '#launch-more' }] },
};

/** The keys that move the list when ArrowDown / j do not (the to-do panel has no focus in a column: Space d j moves it). */
export const MOVE_KEYS = { 'todo-toggle': [[' ', 'd', 'j']] };

/** Text typed after the view opens, for lists that fill from a query. */
export const TYPE_AFTER_OPEN = { 'memory-search': 'ab', 'files-find': 'a', 'graph-open': 't' };

/** The fake daemon's fixtures plus the file reads it does not serve, so the explorer and Find file have rows. */
export function walkFixtures(base) {
  return {
    ...base,
    GetDaemonCapabilities: () => ({ recursive_dag_inspection: true }),
    ListProjectDirectory: (p) => ({ root: '/tmp/fake-project', path: p.path ?? '', truncated: false, entries: [
      { name: 'src', path: 'src', kind: 'dir', size: 0, hidden: false, ignored: false },
      { name: 'README.md', path: 'README.md', kind: 'file', size: 120, hidden: false, ignored: false },
      { name: 'notes.txt', path: 'notes.txt', kind: 'file', size: 40, hidden: false, ignored: false },
    ] }),
    OperatorTodoList: () => ({ open: [0, 1, 2].map((n) => ({
      id: `00000000-0000-4000-8000-00000000030${n}`, scope: 'operator', project_id: null, title: `Walk to-do ${n}`, how: 'do it', link: null,
      priority: 3, status: 'open', closed_by: null, created_at: '2026-10-20T10:00:00.000000000Z', closed_at: null,
    })), recently_closed: [], truncated: false }),
    ListProjectFiles: () => ({ root: '/tmp/fake-project', paths: ['README.md', 'notes.txt', 'src/a.ts', 'src/b.ts'], truncated: false, source: 'walk' }),
  };
}

/** Answers for stubbed commands (the memory index is not a fixture of the fake daemon). */
export const WALK_STUBS = {
  // local #330: the recursive DAG browser's inventory and the graph review's starter picker are not fixtures of the fake daemon.
  graph_list_recursive: [0, 1, 2].map((n) => ({
    id: `00000000-0000-4000-8000-00000000040${n}`, title: `Walk graph ${n}`, status: 'running', execution_mode: 'fake',
    updated_at: `2026-10-20T10:0${n}:00.000000000Z`,
  })),
  dag_get_graph: (a) => ({ id: a.graphId, title: 'Walk graph', status: 'running', tasks: [] }),
  graph_starters: [
    { name: 'pipeline', label: 'Pipeline', description: 'Plan, build, review', use_cases: ['a feature'] },
    { name: 'fan-out', label: 'Fan out', description: 'Parallel workers', use_cases: ['research'] },
    { name: 'review', label: 'Review loop', description: 'Build then review', use_cases: [] },
  ],
  memory_search: [
    { path: 'memory/one.md', start_line: 1, end_line: 3, score: 0.9, snippet: 'first fact', source: 'memory' },
    { path: 'memory/two.md', start_line: 4, end_line: 8, score: 0.7, snippet: 'second fact', source: 'memory' },
  ],
};

/** In the page: what the accessibility wiring says right now. */
export const readWiring = (allowUnfocused = false) => `(() => {
  const vis = (e) => !!e && e.checkVisibility?.({ checkOpacity: false, checkVisibilityCSS: true }) && e.getClientRects().length > 0;
  const txt = (n) => (n?.innerText ?? n?.textContent ?? '').replace(/\\s+/g, ' ').trim().slice(0, 60);
  const active = document.activeElement;
  // the focus owner: the focused element, or the nearest ancestor that carries the pointer
  let owner = null;
  for (let e = active; e && e !== document.body; e = e.parentElement) if (e.hasAttribute('aria-activedescendant')) { owner = e; break; }
  // A panel that holds no focus (the to-do column) still points at its row: report it, flagged unfocused.
  let unfocused = false;
  if (!owner && ${allowUnfocused}) { owner = [...document.querySelectorAll('[aria-activedescendant]')].find((e) => vis(e) && e.id !== 'session-list') ?? null; unfocused = !!owner; }
  const lists = [...document.querySelectorAll('[role=listbox],[role=tree],[role=grid]')].filter(vis);
  const target = owner ? document.getElementById(owner.getAttribute('aria-activedescendant')) : null;
  const list = target?.closest('[role=listbox],[role=tree],[role=grid]') ?? null;
  const rows = list ? [...list.querySelectorAll('[role=option],[role=treeitem],[role=row]')].filter(vis) : [];
  // The controls inside the pointed row: each must be named and reachable (Tab), and no option may hold one at all.
  const nameOf = (e) => (e.getAttribute('aria-label') || [...(e.labels ?? [])].map(txt).join(' ') || txt(e) || e.getAttribute('title') || '').trim();
  const controls = target ? [...target.querySelectorAll('button,input,select,textarea,a[href],[tabindex]')].filter(vis).map((e) => ({
    tag: e.tagName.toLowerCase(), name: nameOf(e), tabbable: !e.disabled && e.tabIndex >= 0, disabled: !!e.disabled,
  })) : [];
  // Every visible child of a grid row is a cell (the label is its rowheader; a <td> / <th> is one by itself).
  const cellRoles = ['gridcell', 'rowheader', 'cell', 'columnheader'];
  const badCells = target && target.getAttribute('role') === 'row'
    ? [...target.children].filter((c) => vis(c) && !cellRoles.includes(c.getAttribute('role') ?? (c.tagName === 'TD' ? 'cell' : c.tagName === 'TH' ? 'columnheader' : ''))).length : 0;
  const selected = rows.filter((r) => r.getAttribute('aria-selected') === 'true');
  const hi = list ? list.querySelector('.sel,.selected') : null;
  return {
    active: active ? active.tagName.toLowerCase() + (active.id ? '#' + active.id : '') + (active.getAttribute('role') ? '[' + active.getAttribute('role') + ']' : '') : null,
    owner: owner ? owner.tagName.toLowerCase() + (owner.id ? '#' + owner.id : '') + (owner.getAttribute('role') ? '[' + owner.getAttribute('role') + ']' : '') : null,
    ownerRole: owner?.getAttribute('role') ?? null, unfocused,
    expanded: owner?.getAttribute('aria-expanded') ?? null,
    controls: owner?.getAttribute('aria-controls') ?? null,
    pointer: owner?.getAttribute('aria-activedescendant') ?? null,
    rowControls: controls, badCells, targetVisible: vis(target), targetRole: target?.getAttribute('role') ?? null, targetSelected: target?.getAttribute('aria-selected') ?? null,
    targetText: txt(target), highlightText: txt(hi), rows: rows.length, selectedCount: selected.length,
    listName: list ? (list.getAttribute('aria-label') ?? txt(document.getElementById(list.getAttribute('aria-labelledby') ?? ''))) : null,
    controlsMatches: owner ? (owner.getAttribute('aria-controls') === null || owner.getAttribute('aria-controls') === list?.id) : null,
    unnamedLists: lists.filter((l) => !(l.getAttribute('aria-label') || l.getAttribute('aria-labelledby'))).length,
    lists: lists.length,
  };
})()`;

/**
 * The verdict for one view from the reading before and after the move key. Pure.
 * `ok` means: a pointer exists, it names the one selected visible row of a named list, the highlight is that row, a combobox
 * owner controls the list, and a list with two or more rows moved the pointer.
 */
export function judgeWiring(before, after) {
  const problems = [];
  if (!after.owner) problems.push('no focused element or ancestor carries aria-activedescendant');
  else {
    if (!after.pointer) problems.push('empty aria-activedescendant');
    if (!after.targetVisible) problems.push('the pointer names no visible element');
    if (!['option', 'treeitem', 'row'].includes(after.targetRole)) problems.push(`the pointer names a ${after.targetRole ?? 'plain'} element, not an option`);
    if (after.targetSelected !== 'true') problems.push('the pointed row is not aria-selected');
    if (after.selectedCount !== 1) problems.push(`${after.selectedCount} rows are aria-selected`);
    if (after.highlightText && after.targetText && after.highlightText !== after.targetText) problems.push(`the highlighted row ("${after.highlightText}") is not the pointed one ("${after.targetText}")`);
    if (!after.listName) problems.push('the list has no accessible name');
    if (after.controlsMatches === false) problems.push('aria-controls does not name the list');
    if (after.owner.includes('input') && after.ownerRole !== 'combobox') problems.push('a filter input is not role=combobox');
    if (after.ownerRole === 'combobox' && after.expanded !== 'true') problems.push('the combobox is not aria-expanded');
    // Controls inside the pointed row (local #334): an option may hold none; a row's must be named and reachable by Tab.
    const held = after.rowControls ?? [];
    if (after.targetRole === 'option' && held.length) problems.push(`an option holds ${held.length} control(s), which a screen reader may flatten`);
    const unnamed = held.filter((c) => !c.name);
    if (unnamed.length) problems.push(`${unnamed.length} control(s) in the pointed row have no accessible name`);
    const unreachable = held.filter((c) => !c.tabbable && !c.disabled);
    if (unreachable.length) problems.push(`${unreachable.length} control(s) in the pointed row cannot be reached by Tab`);
    if (after.badCells) problems.push(`${after.badCells} child(ren) of the pointed row are not cells`);
    if (after.rows >= 2 && before?.pointer === after.pointer) problems.push('the move key did not move the pointer');
  }
  return { ok: problems.length === 0, problems, moved: !!before && before.pointer !== after.pointer, rows: after.rows };
}

const sleepMs = (ms) => new Promise((r) => setTimeout(r, ms));

/** Walk the views one at a time; rows are `{ id, title, opened, before, after, verdict }`. */
/** `seed`: view id -> `(page) => …` run in the page before the view opens (Notifications are the page's own, not a daemon read). */
export async function walkLists(a, views, { lib, reload, sessionTitle = 'Fake waiting worker', keys = ['ArrowDown', 'j'], seed = {} }) {
  const rows = [];
  for (const v of views) {
    const row = { id: v.id, title: v.title };
    try {
      await reload(a.page);
      await sleepMs(400);
      await lib.openSessionByTitle(a.page, sessionTitle);
      await seed[v.id]?.(a.page);
      const r = await lib.openPaletteAction(a.page, v.title, { settleMs: 700 });
      row.opened = r.opened;
      if (!r.opened) { row.why = r.why; rows.push(row); continue; }
      for (const step of EXTRA_VIEWS[v.id]?.pre ?? []) {
        if (step.key) await a.page.key(step.key); else await lib.clickSel(a.page, step.click, 300);
        await sleepMs(300);
      }
      if (TYPE_AFTER_OPEN[v.id]) { await a.page.keys(TYPE_AFTER_OPEN[v.id]); await sleepMs(500); }
      row.before = await a.page.eval(readWiring(v.id in MOVE_KEYS));
      let after = row.before;
      for (const seq of MOVE_KEYS[v.id] ?? keys.map((k) => [k])) {
        for (const k of seq) await a.page.key(k);
        const k = seq.join('');
        await sleepMs(250);
        after = await a.page.eval(readWiring(v.id in MOVE_KEYS));
        row.key = k;
        if (after.pointer !== row.before.pointer || after.rows < 2) break;
      }
      row.after = after;
      row.verdict = judgeWiring(row.before, after);
      row.empty = after.rows === 0 && !after.owner; // nothing drawn in this fixture: there is nothing to announce
    } catch (e) { row.opened = false; row.why = String(e.message ?? e).split('\n')[0]; }
    rows.push(row);
  }
  return rows;
}

export default async function flow({ lib, out }) {
  const { reloadPage } = await import('./audit.mjs');
  const { ACTIONS } = await import('../dist/registry.js');
  const all = [...LIST_VIEWS, ...Object.keys(EXTRA_VIEWS)];
  const views = (process.env.VIEWS ? all.filter((id) => process.env.VIEWS.split(',').includes(id)) : all).map((id) => {
    const act = ACTIONS.find((x) => x.id === (EXTRA_VIEWS[id]?.action ?? id));
    if (!act) throw new Error(`LIST_VIEWS names ${id}, which the app no longer registers`);
    return { id, title: act.title };
  });
  // keyFixtures: several rows for every keyboard list, so the move key can move (audit-fixtures.mjs)
  const { KEY_LOCAL, keyFixtures, seedNotices } = await import('./audit-fixtures.mjs');
  const a = await lib.open({ width: 1280, height: 800, vim: true, stubs: WALK_STUBS, local: KEY_LOCAL, fixtures: keyFixtures(lib.FIXTURES) });
  try {
    await sleepMs(500);
    const rows = await walkLists(a, views, { lib, reload: reloadPage, seed: { 'open-notifications': (page) => seedNotices(page, 3) } });
    for (const r of rows) {
      console.log(`${r.id.padEnd(20)} ${r.opened ? (r.empty ? 'empty' : r.verdict.ok ? 'ok  ' : 'FAIL') : 'skip'} ${r.opened ? `rows ${r.after.rows} key ${r.key} owner ${r.after.owner}` : r.why}${r.opened && !r.verdict.ok ? ` :: ${r.verdict.problems.join('; ')}` : ''}`);
    }
    writeFileSync(join(out, 'listbox-walk.json'), JSON.stringify(rows, null, 1));
  } finally { await a.close(); }
}
