// The ? help lists a dialog's keys by an explicit table id (data-key-table), not by guessing from its title (local #335:
// "Sort sessions" is the Sort Picker). The real Sort, Theme, Project and Label pickers are opened here, help is opened
// from each, and the Contextual view starts with that table's first row. Then the source is walked so a table id that
// names no OVERLAY_TABLES entry (a typo), or a picker whose title spells no table and sets no id, cannot land.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { doc, mount } from './fake-doc.mjs';

mount('div', 'toast');
mount('select', 'project-filter');
doc.documentElement.style = { removeProperty() {}, setProperty() {} };
const { OVERLAY_TABLES } = await import('../dist/overlay-help.generated.js');
const { overlayNames, openKeyHelp } = await import('../dist/keyhelp.js');
const { overlayRows } = await import('../dist/keyhelp-model.js');
const { openPicker, openForm } = await import('../dist/pickers.js');
const { openThemePicker } = await import('../dist/themes.js');
const { state } = await import('../dist/state.js');
const { runSessionList } = await import('../dist/actions/sessionlist-run.js');

const snap = {
  open: { status: 'Running', readOnly: false, pauseLevel: 'none', hasQuestion: false },
  cursorOnRow: true, rowCount: 3, attentionRows: 0, view: 'active', vimKeys: true, hideTools: false,
  showFinished: false, modalOpen: false, event: null,
};
const tableOf = (name) => OVERLAY_TABLES.find((t) => t.name === name);
const dialogs = () => doc.querySelectorAll('dialog').filter((d) => d.open && d.id !== 'keys-help');

/** Focus the dialog's first focusable child (what the operator has focused), as help opens from there. */
function focusInside(dlg) {
  const target = dlg.querySelector('input') ?? dlg.querySelector('ul') ?? dlg;
  target.focus();
  return target;
}

test('the Sort, Theme, Project and Label pickers each resolve to their key table', () => {
  state.projects = [{ id: 'p1', name: 'Alpha' }];
  state.labels = [{ id: 'l1', name: 'bug', description: null, color: '#ff0000' }];
  state.sessions = [{ id: 's1', title: 'one', project_id: null, status: 'Running', provider: 'Claude', labels: [] }];
  state.selectedId = 's1';
  const opens = {
    'Sort Picker': () => runSessionList('choose-sort', { source: 'key' }),
    'Theme Picker': () => openThemePicker(),
    'Project Picker': () => runSessionList('choose-project', { source: 'key' }),
    'Label Picker': () => runSessionList('choose-label', { source: 'key' }),
  };
  for (const [table, open] of Object.entries(opens)) {
    open();
    const dlg = dialogs().at(-1);
    assert.ok(dlg, `${table}: a dialog opened`);
    const names = overlayNames(focusInside(dlg));
    assert.deepEqual(names, [table], `${table}: help maps the dialog to its table`);
    const rows = overlayRows(names);
    assert.ok(rows.length > 0, `${table}: the table has rows`);
    assert.deepEqual(rows[0].keys, [tableOf(table).rows[0].keys], `${table}: first row`);
    dlg.close();
  }
});

test('a dialog that names its table beats the title; one that does not falls back to its title', () => {
  openForm({ title: 'Edit project', keyTable: 'Project Form', submit: 'Save', fields: [], validate: () => null, onSubmit: async () => {} });
  const form = dialogs().at(-1);
  assert.deepEqual(overlayNames(focusInside(form)), ['Project Form']);
  form.close();
  openPicker({ title: 'Model Picker', hint: 'x', items: [], onPick() {} });
  assert.deepEqual(overlayNames(focusInside(dialogs().at(-1))), ['model picker'], 'the title fallback is kept for a dialog with no id');
});

// ---------- the source walk ----------

const SRC = fileURLToPath(new URL('../src/', import.meta.url));
const files = (dir = SRC) => readdirSync(dir).flatMap((f) => {
  const p = dir + f;
  return statSync(p).isDirectory() ? files(`${p}/`) : p.endsWith('.ts') && !p.endsWith('.generated.ts') ? [p] : [];
});
const names = new Set(OVERLAY_TABLES.map((t) => t.name));

test('every key-table id in the source names a real OVERLAY_TABLES entry', () => {
  const found = [];
  for (const f of files()) {
    const text = readFileSync(f, 'utf8');
    for (const m of text.matchAll(/keyTable: '([^']+)'/g)) found.push([f, m[1]]);
    for (const m of text.matchAll(/pickDialog\('[^']+', [^,)]+(?:\([^)]*\))?[^,)]*, '([^']+)'\)/g)) found.push([f, m[1]]);
    for (const m of text.matchAll(/\['[#.][\w-]+', \[((?:'[^']+',? ?)+)\]\]/g)) for (const n of m[1].match(/'([^']+)'/g)) found.push([f, n.slice(1, -1)]);
  }
  assert.ok(found.length >= 20, `found the table ids (${found.length})`);
  for (const [f, n] of found) assert.ok(names.has(n), `${f.replace(SRC, 'src/')}: "${n}" is not an OVERLAY_TABLES name`);
});

test('every openPicker call names its table, unless it is on the list of pickers with no key table of their own', () => {
  // These pickers have no table in the TUI's overlay catalog. A new picker must name its table (keyTable) or be added here on purpose.
  const NO_TABLE = [
    "src/actions/sessionlist-run.ts: 'Attention queue'",
    'src/actions/operatorconfig.ts: `${role.label} runs against`',
    "src/actions/operatorconfig.ts: 'Route a model role to a custom provider'",
    "src/actions/operatorconfig.ts: 'Custom providers'",
    "src/actions/operatorconfig.ts: 'Message bridges'",
    "src/actions/operatorconfig.ts: `Claude hooks · ${view.path || '~/.claude/settings.json'}${view.exists ? '' : ' (not created yet)'}`",
    'src/prompt-creator.ts: `Prompt creator model: ${provider}`',
    'src/issues.ts: `${depLabel(v.direction as DepDirection)}: pick the other Issue`',
    'src/actions/sessionlist-run.ts: title',
  ];
  const missing = [];
  let seen = 0;
  for (const f of files()) {
    const text = readFileSync(f, 'utf8');
    for (const m of text.matchAll(/openPicker\(\{\n\s+title(?:: ([^\n]+?))?,\n(\s+keyTable:)?/g)) {
      seen++;
      if (m[2]) continue;
      const entry = `${f.replace(SRC, 'src/')}: ${m[1] ?? 'title'}`;
      if (!NO_TABLE.includes(entry)) missing.push(entry);
    }
  }
  assert.ok(seen >= 8, `found the openPicker calls (${seen})`);
  assert.deepEqual(missing, [], 'an openPicker call with no keyTable');
});
