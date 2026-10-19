// Every desktop action in the registry is reachable by mouse or menu (hub #3016, Issue #117).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import * as reg from '../dist/registry.js';
import { bySection, htmlButtonActions, parityRows, reachability, renderSectionTable } from '../scripts/reachability-lib.mjs';
import { load, loadBySection } from '../scripts/reachability.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const read = (p) => readFileSync(here(p), 'utf8');
const real = () => ({
  html: read('../static/index.html'),
  palette: read('../src/palette.ts'),
  pageSources: ['../src/menus.ts', '../src/issues.ts', '../src/actions/nav-run.ts'].map(read),
});

test('every registry action is listed on a mouse or menu surface', async () => {
  const { rows, problems } = await load();
  assert.deepEqual(problems, []);
  assert.equal(rows.length, reg.ACTIONS.length);
  for (const r of rows) assert.ok(r.surfaces.length > 0, `${r.id} has no surface`);
});

test('right-click-only actions are in a wired context menu; the rest are in the palette', async () => {
  const { rows } = await load();
  for (const r of rows) {
    if (r.contextOnly) assert.ok(r.surfaces.some((s) => s.startsWith('context:')), `${r.id} is context-only without a context menu`);
    else assert.ok(r.surfaces.includes('palette'), `${r.id} is missing from the palette`);
  }
});

test('the palette opens by mouse: toolbar button, View menu and a clickable list', () => {
  assert.ok(htmlButtonActions(real().html).includes('palette'));
  const view = reg.appMenuModel('other', { open: null, cursorOnRow: false, rowCount: 0, attentionRows: 0, view: 'active', vimKeys: false, hideTools: false, showFinished: false, modalOpen: false, event: null })
    .find((m) => m.id === 'view');
  assert.ok(view.items.some((n) => n.kind === 'item' && n.id === 'act:palette'));
});

test('every toolbar data-action button names a registered action', () => {
  const known = new Set(reg.ACTIONS.map((a) => a.id));
  const ids = htmlButtonActions(real().html);
  assert.ok(ids.length >= 10);
  for (const id of ids) assert.ok(known.has(id), id);
});

test('the check fails on a missing palette button, palette click wiring and context listener', () => {
  const ok = reachability(reg, real());
  assert.deepEqual(ok.problems, []);
  const noButton = reachability(reg, { ...real(), html: real().html.replace(/data-action="palette"/, '') });
  assert.ok(noButton.problems.some((p) => p.includes('data-action="palette"')));
  const noClick = reachability(reg, { ...real(), palette: '' });
  assert.ok(noClick.problems.some((p) => p.includes('click handler')));
  assert.ok(noClick.rows.some((r) => r.surfaces.length === 0 && !r.contextOnly) || noClick.problems.length >= 1);
  const noCtx = reachability(reg, { ...real(), pageSources: [] });
  assert.ok(noCtx.problems.some((p) => p.includes('contextmenu listener')));
  assert.ok(noCtx.problems.some((p) => p.startsWith('copy-event')), 'a right-click-only action becomes unreachable');
});

test('the check fails on an action no surface lists', () => {
  const ghost = { id: 'ghost-action', title: 'Ghost', group: 'view', noMenu: true };
  const fake = {
    ...reg,
    ACTIONS: [...reg.ACTIONS, ghost],
    paletteEntries: (s, p) => reg.paletteEntries(s, p).filter((e) => e.id !== 'ghost-action'),
  };
  const r = reachability(fake, real());
  assert.ok(r.problems.some((p) => p.startsWith('ghost-action')), r.problems.join('\n'));
});

test('by-section: one row per PARITY.md ## section, every linked action reachable', async () => {
  const { sections, problems, parityRows: n } = await loadBySection();
  assert.deepEqual(problems, []);
  const headings = read('../PARITY.md').split('\n').filter((l) => l.startsWith('## ')).map((l) => l.slice(3).trim());
  assert.deepEqual(sections.map((s) => s.section), headings);
  assert.equal(sections.reduce((a, s) => a + s.rows, 0), n);
  for (const s of sections) assert.equal(s.rows, s.binding + s.module + s.none, s.section);
  // The keyed sections are bound to real actions, not just module neighbours.
  for (const name of ['Normal Mode Keybindings', 'Command Mode']) {
    assert.ok(sections.find((s) => s.section === name).direct.actions > 20, name);
  }
  assert.match(renderSectionTable(sections), /\| Command Mode \| 77 \|/);
});

test('by-section: an action with no surface is a problem for the section that links it', () => {
  const rows = parityRows('## S\n\n| `key:j` | key | `j` | `x.rs` | done | `src/y.ts` | n |\n', (l) => l.split('|').slice(1, -1).map((c) => c.trim()), (c) => c.replace(/`/g, ''));
  const keys = { j: { action: 'cursor-down' } };
  const ok = bySection(rows, [{ id: 'cursor-down', group: 'go', title: 'Down', surfaces: ['menu'] }], keys, {}, {});
  assert.deepEqual(ok.problems, []);
  assert.equal(ok.sections[0].binding, 1);
  const bad = bySection(rows, [{ id: 'cursor-down', group: 'go', title: 'Down', surfaces: [] }], keys, {}, {});
  assert.equal(bad.problems.length, 1);
});
