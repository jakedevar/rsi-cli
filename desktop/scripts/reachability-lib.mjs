// Mechanical mouse/menu reachability of the desktop action registry (hub #3016,
// Issue #117). Pure: takes the built registry module and the page sources, so
// tests can feed it a broken registry. The registry is the one action list
// (src/registry.ts); a newcomer without Vim keys reaches an action through
//   - the native app menu bar (appMenuModel),
//   - a right-click menu (contextMenuModel) with a contextmenu listener in the page,
//   - a toolbar button (data-action in static/index.html, wired by wireActionButtons),
//   - or the command palette, itself opened by the "Actions…" button, the
//     View menu item and a clickable list.
// "Reachable" means a mouse surface LISTS the action and its DOM wiring exists.
// Whether it is enabled is state-dependent (registry rules, tested elsewhere):
// a disabled item says why instead of vanishing. An action no surface lists, or
// whose surface has no DOM wiring, is reported as a problem.

/** UI snapshots over which the menus are built (items are listed in every state; enabled state varies). */
export function snapshots() {
  const base = {
    open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
    cursorOnRow: true, rowCount: 3, attentionRows: 1, view: 'active',
    vimKeys: true, hideTools: false, showFinished: false, modalOpen: false, event: { foldable: true, hasText: true },
  };
  const open = (o) => ({ ...base, open: { ...base.open, ...o } });
  return [
    base,
    open({ pauseLevel: 'soft' }),
    open({ pauseLevel: 'hard' }),
    open({ hasQuestion: true }),
    open({ status: 'Completed' }),
    open({ readOnly: true }),
    { ...base, view: 'archived', open: null },
    { ...base, open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, vimKeys: false, event: null },
  ];
}

const PLATFORMS = ['mac', 'other'];
const idsOf = (nodes) => nodes.filter((n) => n.kind === 'item').map((n) => n.id);

/** The `data-action` ids in a static HTML page. */
export function htmlButtonActions(html) {
  return [...html.matchAll(/<button\b[^>]*\bdata-action="([a-z-]+)"/g)].map((m) => m[1]);
}

/**
 * @param {object} reg      dist/registry.js exports (ACTIONS, appMenuModel, contextMenuModel, paletteEntries, unavailable)
 * @param {object} src      { html, palette, menus, pageSources: string[] } file texts
 * @returns {{ rows: object[], problems: string[] }}
 */
export function reachability(reg, src) {
  const snaps = snapshots();
  const problems = [];
  const menuBar = new Map(); // action -> enabled in some snapshot
  for (const p of PLATFORMS) {
    for (const s of snaps) {
      for (const m of reg.appMenuModel(p, s)) {
        for (const n of m.items) {
          if (n.kind !== 'item') continue;
          const mm = /^act:([a-z-]+)$/.exec(n.id);
          if (mm) menuBar.set(mm[1], true);
        }
      }
    }
  }
  const kinds = [...new Set(reg.ACTIONS.flatMap((a) => a.contexts ?? []))];
  const ctx = new Map(); // action -> Set(kind) enabled somewhere
  for (const k of kinds) {
    for (const s of snaps) {
      for (const n of reg.contextMenuModel(k, s)) {
        const mm = n.kind === 'item' ? /^ctx:([a-z-]+)$/.exec(n.id) : null;
        if (!mm) continue;
        if (!ctx.has(mm[1])) ctx.set(mm[1], new Set());
        ctx.get(mm[1]).add(k);
      }
    }
  }
  const buttons = new Set(htmlButtonActions(src.html));
  const paletteOn = new Set();
  for (const p of PLATFORMS) for (const s of snaps) for (const e of reg.paletteEntries(s, p)) paletteOn.add(e.id);

  // Surface prerequisites: how a mouse gets to the palette and to each context menu.
  const paletteButton = buttons.has('palette');
  const paletteInMenu = menuBar.has('palette');
  const paletteClick = /list\.addEventListener\('click'/.test(src.palette) && /source: 'palette'/.test(src.palette);
  if (!paletteButton) problems.push('no toolbar button with data-action="palette" (Actions…)');
  if (!paletteInMenu) problems.push('the palette action is not in the app menu bar');
  if (!paletteClick) problems.push('palette.ts has no click handler on its list that dispatches with source palette');
  const paletteOpensByMouse = paletteButton && paletteClick;
  // A contextmenu listener whose file builds that kind's menu: directly or through menus.ts popup(kind).
  const domContext = (k) => src.pageSources.some((t) => t.includes('contextmenu')
    && (t.includes(`contextMenuModel('${k}'`) || t.includes(`popup('${k}')`)));
  for (const k of kinds) if (!domContext(k)) problems.push(`context kind ${k} has actions but no contextmenu listener builds its menu`);
  for (const b of buttons) if (!reg.ACTIONS.some((a) => a.id === b)) problems.push(`button data-action="${b}" is not a registered action`);

  const rows = reg.ACTIONS.map((a) => {
    const surfaces = [];
    if (menuBar.get(a.id)) surfaces.push('menu');
    if ((ctx.get(a.id)?.size ?? 0) > 0) surfaces.push(...[...ctx.get(a.id)].filter(domContext).map((k) => `context:${k}`));
    if (buttons.has(a.id)) surfaces.push('button');
    if (!a.contextOnly && paletteOpensByMouse && paletteOn.has(a.id)) surfaces.push('palette');
    const row = { id: a.id, title: a.title, group: a.group, surfaces, contextOnly: Boolean(a.contextOnly) };
    if (surfaces.length === 0) {
      const why = [];
      if (a.contextOnly) why.push('context-only and no wired context menu lists it');
      else if (!paletteOpensByMouse) why.push('the palette is not mouse-reachable');
      else why.push('not listed by the palette or any menu');
      problems.push(`${a.id} (${a.title}): no mouse or menu surface${why.length ? `; ${why.join('; ')}` : ''}`);
    }
    if (menuBar.has(a.id) && a.noMenu) problems.push(`${a.id}: noMenu action is in the menu bar`);
    if (!menuBar.has(a.id) && !a.contextOnly && !a.noMenu) problems.push(`${a.id}: should be in the menu bar (no noMenu) but is not`);
    return row;
  });
  return { rows, problems };
}

export function renderTable(rows) {
  const out = ['id\tgroup\tsurfaces'];
  for (const r of rows) out.push(`${r.id}\t${r.group}\t${r.surfaces.join(',') || 'NONE'}`);
  return out.join('\n');
}

// ---------- by PARITY section (hub #3016, Issue #142) ----------
//
// One row per `##` section of PARITY.md. A PARITY row is a TUI behaviour; the
// registry action that does it on the desktop is found three ways, strongest first:
//   binding  the row's TUI key or `:` command is bound to the action
//            (KEY_BINDINGS / COMMAND_BINDINGS, also the keys and commands named in
//            an overlay row's title, e.g. "Settings Pane (`<Space>,` or `:set`)");
//   module   the row's desktop location names a slice module (src/actions/<x>.ts,
//            <x>-run.ts, <x>-spec.ts) and the row is reached by that slice's actions;
//   none     no action link: keys typed inside a field or an overlay, and TUI render
//            modules. They have no mouse entry point of their own; the section row
//            counts them and the report names what reaches them.
// A linked action with no mouse or menu surface is a problem.

/** `|`-split rows of the PARITY.md tables, each with its `##` section. */
export function parityRows(markdown, splitRow, unwrap) {
  const out = [];
  let section = '';
  for (const line of markdown.split('\n')) {
    if (line.startsWith('## ')) section = line.slice(3).trim();
    if (!line.startsWith('| `')) continue;
    const c = splitRow(line);
    out.push({ section, id: unwrap(c[0]), kind: unwrap(c[1]), binding: c[2] ?? '', location: c[5] ?? '' });
  }
  return out;
}

/** Slice module stem -> action ids, from the generated slice index and each module's SLICE. */
export function stemActionMap(stems, slices) {
  const map = {};
  for (const s of stems) map[s.replace(/-spec$/, '')] = slices[s].actions.map((a) => a.id);
  return map;
}

/** Action ids a row's TUI binding, or the bindings in its title, name. */
export function bindingActions(row, keyBindings, commandBindings) {
  const ids = new Set();
  const text = row.binding.replace(/`/g, '');
  for (const m of text.matchAll(/(?:^|[\s(,])(:[a-z][a-z0-9_-]*)/g)) {
    const b = commandBindings[m[1].slice(1)];
    if (b) ids.add(b.action);
  }
  const cands = ['key', 'global', 'leader'].includes(row.kind) ? [text.trim()] : [...row.binding.matchAll(/`([^`]+)`/g)].map((m) => m[1].trim());
  for (const k of cands) {
    const b = keyBindings[k];
    if (b) ids.add(b.action);
  }
  return [...ids];
}

export function moduleActions(row, stemActions) {
  const ids = new Set();
  for (const f of row.location.match(/src\/[\w/.-]+\.ts/g) ?? []) {
    const stem = f.replace(/^src\/(actions\/)?/, '').replace(/\.ts$/, '').replace(/-(run|spec)$/, '');
    for (const id of stemActions[stem] ?? []) ids.add(id);
  }
  return [...ids];
}

/**
 * @param {object[]} rows       parityRows() output
 * @param {object[]} reachRows  reachability() rows (id, group, surfaces)
 */
export function bySection(rows, reachRows, keyBindings, commandBindings, stemActions) {
  const reach = new Map(reachRows.map((r) => [r.id, r]));
  const sections = new Map();
  const problems = [];
  for (const r of rows) {
    const s = sections.get(r.section) ?? { section: r.section, rows: 0, binding: 0, module: 0, none: 0, kinds: {}, unlinked: [], actions: new Set(), moduleOnly: new Set() };
    sections.set(r.section, s);
    s.rows++;
    s.kinds[r.kind] = (s.kinds[r.kind] ?? 0) + 1;
    const b = bindingActions(r, keyBindings, commandBindings);
    if (b.length) {
      s.binding++;
      for (const id of b) s.actions.add(id);
      continue;
    }
    const m = moduleActions(r, stemActions);
    if (m.length) {
      s.module++;
      for (const id of m) s.moduleOnly.add(id);
    } else {
      s.none++;
      s.unlinked.push(r.id);
    }
  }
  const entry = (ids) => {
    const surfaces = new Set();
    const menus = new Set();
    let unreachable = 0;
    for (const id of ids) {
      const a = reach.get(id);
      if (!a) { problems.push(`action ${id} is linked to a PARITY row but is not in the registry`); continue; }
      if (!a.surfaces.length) unreachable++;
      for (const x of a.surfaces) surfaces.add(x);
      if (a.surfaces.includes('menu')) menus.add(a.group);
    }
    const examples = [...ids].map((id) => reach.get(id)).filter((a) => a?.surfaces.includes('menu'))
      .slice(0, 3).map((a) => `${a.group[0].toUpperCase()}${a.group.slice(1)} ▸ ${a.title}`);
    return { surfaces: [...surfaces].sort(), menus: [...menus].sort(), actions: ids.size, unreachable, examples };
  };
  const result = [...sections.values()].map((s) => {
    for (const id of s.actions) s.moduleOnly.delete(id);
    const direct = entry(s.actions);
    const viaModule = entry(s.moduleOnly);
    if (direct.unreachable + viaModule.unreachable) problems.push(`section ${s.section}: ${direct.unreachable + viaModule.unreachable} linked action(s) have no mouse or menu surface`);
    return { section: s.section, rows: s.rows, kinds: s.kinds, binding: s.binding, module: s.module, none: s.none, unlinked: s.unlinked, direct, viaModule };
  });
  return { sections: result, problems };
}

export function renderSectionTable(sections) {
  const out = ['| PARITY section | rows | via binding | via module | no action | mouse/menu entry points (binding-linked actions) |', '| --- | --- | --- | --- | --- | --- |'];
  for (const s of sections) {
    const surf = (x) => [x.menus.length ? `menus ${x.menus.join('/')}` : '', ...x.surfaces.filter((y) => y !== 'menu')].filter(Boolean).join(', ');
    const e = s.direct.actions ? `${s.direct.actions} actions (${surf(s.direct)}); e.g. ${s.direct.examples.join('; ')}` : 'none bound directly';
    const m = s.viaModule.actions ? `; +${s.viaModule.actions} via modules (${surf(s.viaModule)})` : '';
    out.push(`| ${s.section} | ${s.rows} | ${s.binding} | ${s.module} | ${s.none} | ${e}${m} |`);
  }
  return out.join('\n');
}
