// Pure logic behind scripts/parity.mjs: parse docs/keybindings.md into feature
// rows, merge the hand-kept status sidecar and render desktop/PARITY.md.
// No file system access here except through the callbacks the caller passes in,
// so tests/parity.test.mjs can drive it with fixtures. Node built-ins only.

export const STATUSES = ['missing', 'partial', 'done', 'substitute'];

/** Row kinds, in the order they are documented in the README. */
export const KINDS = ['section', 'global', 'key', 'leader', 'cmd', 'overlay', 'overlay-ref', 'module'];

/** The `##` titles that switch the area the parser is in. */
const AREAS = [
  [/^global keybindings/i, 'global'],
  [/^normal mode keybindings/i, 'normal'],
  [/^command mode/i, 'command'],
  [/^overlay key reference/i, 'overlay-ref'],
  [/^overlay keybindings/i, 'overlay'],
];

/** Default TUI module per row kind (a file under crates/rsi/src). */
export const DEFAULT_MODULES = {
  global: 'crates/rsi/src/keybindings.rs',
  key: 'crates/rsi/src/action_registry.rs',
  leader: 'crates/rsi/src/action_registry.rs',
  cmd: 'crates/rsi/src/commands.rs',
};

/**
 * Heading-slug -> TUI module hints for overlay and section rows. First match
 * wins. A hint whose file is missing is an error (the caller checks), so a
 * rename in crates/rsi is noticed instead of silently dropping the module.
 */
export const MODULE_HINTS = [
  [/manager-tree/, 'crates/rsi/src/ui/overlay/manager_tree.rs'],
  [/global-manager/, 'crates/rsi/src/ui/overlay/global_manager_workspace.rs'],
  [/settlement/, 'crates/rsi/src/ui/overlay/cohort_settlement.rs'],
  [/harness-manager|^manager-|-manager-|^manager$/, 'crates/rsi/src/ui/overlay/harness_manager.rs'],
  [/terminal/, 'crates/rsi/src/ui/overlay/terminal.rs'],
  [/new-session|continue-prompt|prompt-overlay|launch/, 'crates/rsi/src/ui/overlay/prompt.rs'],
  [/prompt-creator/, 'crates/rsi/src/ui/prompt_creator.rs'],
  [/prompt-preview|skill-preview/, 'crates/rsi/src/ui/overlay/prompt_preview.rs'],
  [/question/, 'crates/rsi/src/ui/overlay/question_modal.rs'],
  [/model-dropdown/, 'crates/rsi/src/ui/widget/model_dropdown.rs'],
  [/switch-model/, 'crates/rsi/src/ui/overlay/model_switch.rs'],
  [/theme-role/, 'crates/rsi/src/ui/overlay/theme_role_editor.rs'],
  [/theme/, 'crates/rsi/src/ui/overlay/theme_picker.rs'],
  [/project-picker/, 'crates/rsi/src/ui/overlay/project_picker.rs'],
  [/project-form/, 'crates/rsi/src/ui/overlay/project_form.rs'],
  [/card-editor/, 'crates/rsi/src/ui/overlay/card_editor.rs'],
  [/sort-picker/, 'crates/rsi/src/ui/overlay/sort_picker.rs'],
  [/archive-zone|trash/, 'crates/rsi/src/ui/overlay/trash_browser.rs'],
  [/file-viewer/, 'crates/rsi/src/file_viewer.rs'],
  [/file-explorer/, 'crates/rsi/src/ui/overlay/file_explorer.rs'],
  [/telescope/, 'crates/rsi/src/ui/overlay/telescope.rs'],
  [/settings-pane|remote-settings/, 'crates/rsi/src/ui/settings.rs'],
  [/keybindings-help/, 'crates/rsi/src/ui/overlay/keybindings_help.rs'],
  [/esp-square/, 'crates/rsi/src/ui/overlay/esp_square.rs'],
  [/dag/, 'crates/rsi/src/ui/overlay/recursive_dag.rs'],
  [/graph/, 'crates/rsi/src/ui/overlay/graph.rs'],
  [/satellite-registry/, 'crates/rsi/src/ui/overlay/satellite_registry.rs'],
  [/legacy-scratch/, 'crates/rsi/src/ui/overlay/legacy_scratch.rs'],
  [/scheduled-jobs/, 'crates/rsi/src/ui/overlay/schedule_browser.rs'],
  [/schedule-form/, 'crates/rsi/src/ui/overlay/schedule_form.rs'],
  [/create-entity|unified-entity/, 'crates/rsi/src/ui/overlay/create_entity_form.rs'],
  [/label-picker/, 'crates/rsi/src/ui/overlay/label_picker.rs'],
  [/label-form/, 'crates/rsi/src/ui/overlay/label_form.rs'],
  [/parent-picker/, 'crates/rsi/src/ui/overlay/parent_picker.rs'],
  [/command-palette/, 'crates/rsi/src/ui/overlay/command_palette.rs'],
  [/notifications|recent-completions/, 'crates/rsi/src/ui/overlay/notification_browser.rs'],
  [/provider-key/, 'crates/rsi/src/ui/overlay/provider_credential_form.rs'],
  [/provider-form/, 'crates/rsi/src/ui/overlay/provider_form.rs'],
  [/message-bridge/, 'crates/rsi/src/ui/overlay/message_bridge_form.rs'],
  [/hook-/, 'crates/rsi/src/ui/overlay/hook_form.rs'],
  [/budget-policy/, 'crates/rsi/src/ui/overlay/budget_policy_form.rs'],
  [/rename-session/, 'crates/rsi/src/ui/overlay/rename_session.rs'],
  [/text-area-background/, 'crates/rsi/src/ui/overlay/text_area_bg_editor.rs'],
  [/daemon-text/, 'crates/rsi/src/ui/overlay/daemon_text_editor.rs'],
  [/to-do/, 'crates/rsi/src/ui/overlay/todo_popup.rs'],
  [/diagnostics/, 'crates/rsi/src/ui/overlay/diagnostics.rs'],
  [/session-info/, 'crates/rsi/src/ui/overlay/session_info.rs'],
  [/rate-session/, 'crates/rsi/src/ui/overlay/rating.rs'],
  [/memory-search/, 'crates/rsi/src/ui/overlay/memory_search.rs'],
  [/ai-command/, 'crates/rsi/src/ui/overlay/ai_command.rs'],
  [/ai-chat/, 'crates/rsi/src/ui/overlay/ai_chat.rs'],
  [/dialectic/, 'crates/rsi/src/ui/overlay/dialectic.rs'],
  [/input-modal/, 'crates/rsi/src/ui/overlay/input_modal.rs'],
  [/fleet/, 'crates/rsi/src/ui/overlay/fleet.rs'],
  [/model-picker/, 'crates/rsi/src/ui/overlay/model_switch.rs'],
  [/issues-workspace/, 'crates/rsi/src/ui/issues.rs'],
  [/theme-selection/, 'crates/rsi/src/ui/theme.rs'],
];

/** Lowercase, non-alphanumerics collapsed to `-`, trimmed. */
export function slug(text) {
  return String(text)
    .toLowerCase()
    .replace(/`/g, '')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

/** Split a markdown table line into trimmed cells (handles `\|` inside code). */
export function splitRow(line) {
  let s = line.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1);
  const cells = [];
  let cur = '';
  for (let i = 0; i < s.length; i++) {
    if (s[i] === '\\' && s[i + 1] === '|') { cur += '|'; i++; continue; }
    if (s[i] === '|') { cells.push(cur.trim()); cur = ''; continue; }
    cur += s[i];
  }
  cells.push(cur.trim());
  return cells;
}

const isSeparator = (cells) => cells.length > 0 && cells.every((c) => /^:?-{2,}:?$|^-+$/.test(c));

/** Text of a cell with every backtick (code-span marker) removed. */
export function unwrap(cell) {
  return cell.replace(/`/g, '').trim();
}

/**
 * Parse keybindings.md into `{ headings, tables }` events in document order.
 * Returns a flat list of items:
 *   {type:'heading', level, text, area, h2, h3}
 *   {type:'row', header:[...], cells:[...], area, h2, h3, h4}
 * `area` follows the `##` titles in AREAS ('other' otherwise).
 */
export function parseKeybindings(markdown) {
  const items = [];
  let area = 'other';
  let h2 = '', h3 = '', h4 = '';
  let fence = false;
  let header = null;
  for (const raw of markdown.split(/\r?\n/)) {
    if (/^\s*```/.test(raw)) { fence = !fence; header = null; continue; }
    if (fence) continue;
    const hm = /^(#{1,4})\s+(.*?)\s*#*\s*$/.exec(raw);
    if (hm) {
      header = null;
      const level = hm[1].length;
      const text = hm[2].trim();
      if (level === 2) {
        h2 = text; h3 = ''; h4 = '';
        area = 'other';
        for (const [re, a] of AREAS) if (re.test(text)) { area = a; break; }
      } else if (level === 3) { h3 = text; h4 = ''; } else if (level === 4) { h4 = text; }
      items.push({ type: 'heading', level, text, area, h2, h3, h4 });
      continue;
    }
    if (!raw.trim().startsWith('|')) {
      if (!raw.trim()) continue; // blank line: a table may not resume, but keep header until text
      header = null;
      continue;
    }
    const cells = splitRow(raw);
    if (isSeparator(cells)) continue;
    if (!header) { header = cells; continue; }
    items.push({ type: 'row', header, cells, area, h2, h3, h4 });
  }
  return items;
}

function dedupe(rows) {
  const seen = new Map();
  for (const r of rows) {
    const n = (seen.get(r.id) ?? 0) + 1;
    seen.set(r.id, n);
    if (n > 1) r.id = `${r.id}~${n}`;
  }
  return rows;
}

function moduleFor(text, hints, exists) {
  const s = slug(text);
  for (const [re, file] of hints) {
    if (re.test(s)) {
      if (!exists(file)) throw new Error(`parity: module hint for "${text}" points at missing file ${file}`);
      return file;
    }
  }
  return '';
}

const clip = (s, n) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);

/** Command words of a command-mode cell, arguments dropped: `:manager board [x]` -> `manager-board`. */
function commandName(cell) {
  const m = /`:([^`[<]*)/.exec(cell);
  return slug(m ? m[1] : cell);
}

/**
 * Build feature rows from parsed keybindings items.
 * `opts.modules` is the list of view module files (relative paths) to list as
 * `module` rows; `opts.exists(rel)` says whether a module file exists.
 * Row: {id, kind, group, binding, module, detail}. `group` is the `##` title.
 */
export function buildRows(items, opts = {}) {
  const exists = opts.exists ?? (() => true);
  const hints = opts.hints ?? MODULE_HINTS;
  const defaults = opts.defaults ?? DEFAULT_MODULES;
  const rows = [];
  const overlayIdx = new Map(); // `${kind}:${h3}` -> row, to count keys
  const fixedModule = (kind) => {
    const f = defaults[kind] ?? '';
    if (f && !exists(f)) throw new Error(`parity: default module for ${kind} missing: ${f}`);
    return f;
  };

  for (const it of items) {
    if (it.type === 'heading') {
      if (it.level < 3) continue;
      const inOverlay = it.area === 'overlay' || it.area === 'overlay-ref';
      if (inOverlay) {
        if (it.level !== 3) continue;
        const kind = it.area;
        const row = {
          id: `${kind}:${slug(it.text)}`, kind, group: it.h2, binding: it.text,
          module: moduleFor(it.text, hints, exists), detail: '', keys: 0,
        };
        rows.push(row);
        overlayIdx.set(`${kind}:${it.text}`, row);
        continue;
      }
      const path = it.level === 4 && it.h3 ? `${it.h3} › ${it.text}` : it.text;
      rows.push({
        id: `section:${slug(it.level === 4 && it.h3 ? `${it.h3} ${it.text}` : it.text)}`,
        kind: 'section', group: it.h2, binding: path,
        module: moduleFor(it.text, hints, exists) || (it.level === 4 ? moduleFor(it.h3, hints, exists) : ''),
        detail: '',
      });
      continue;
    }
    const first = it.header[0]?.toLowerCase() ?? '';
    const c = it.cells;
    if (it.area === 'overlay' || it.area === 'overlay-ref') {
      const row = overlayIdx.get(`${it.area}:${it.h3}`);
      if (row) row.keys += 1;
      continue;
    }
    if (it.area === 'global' && first === 'chord') {
      rows.push({
        id: `global:${slug(unwrap(c[0]))}/${clip(slug(c[1] ?? ''), 40)}`, kind: 'global', group: it.h2,
        binding: unwrap(c[0]), module: fixedModule('global'), detail: `${c[1] ?? ''}: ${c[2] ?? ''}`,
      });
    } else if (it.area === 'normal' && /all normal-mode keys/i.test(it.h3) && first === 'keys') {
      rows.push({
        id: `key:${unwrap(c[0])}`, kind: 'key', group: it.h2, binding: unwrap(c[0]),
        module: fixedModule('key'), detail: `${c[1] ?? ''}`,
      });
    } else if (it.area === 'normal' && /^leader key/i.test(it.h3) && first === 'before' && c.length >= 3) {
      const after = unwrap(c[1]);
      if (after.startsWith('<Space>')) {
        rows.push({
          id: `leader:${after}`, kind: 'leader', group: it.h2, binding: after,
          module: fixedModule('leader'), detail: c[2] ?? '',
        });
      }
    } else if (it.area === 'command' && first === 'command') {
      const aliases = (c[1] ?? '').trim();
      rows.push({
        id: `cmd:${commandName(c[0])}`, kind: 'cmd', group: it.h2,
        binding: aliases ? `${unwrap(c[0])} (${aliases.replace(/`/g, '')})` : unwrap(c[0]),
        module: fixedModule('cmd'), detail: `${c[2] ?? ''}`,
      });
    }
  }
  // A leader row whose key the generated normal table already lists is a duplicate.
  const keyIds = new Set(rows.filter((r) => r.kind === 'key').map((r) => r.id.slice(4)));
  const out = rows.filter((r) => !(r.kind === 'leader' && keyIds.has(r.binding)));

  for (const file of opts.modules ?? []) {
    out.push({
      id: `module:${file.replace(/^crates\/rsi\/src\//, '')}`, kind: 'module', group: 'TUI view modules',
      binding: file.replace(/^crates\/rsi\/src\//, ''), module: file, detail: '',
    });
  }
  return dedupe(out);
}

const SHAPE = '{"version":1,"rows":{<id>:{status,location,note,owner?}}}';

/**
 * Merge the per-slice status files of parity-status.d/ ([{ name, data }], in
 * file order) into one sidecar. Each file is `{"version":1,"rows":{...}}`; an id
 * in two files is an error naming both, so parallel slices never share a row.
 * The result's `sources` maps each id to its file (for error messages).
 */
export function mergeSidecars(files) {
  const rows = {};
  const sources = {};
  const problems = [];
  for (const { name, data } of files) {
    const entries = data && typeof data === 'object' ? data.rows : null;
    if (!entries || typeof entries !== 'object' || Array.isArray(entries)) {
      problems.push(`parity-status.d/${name}: expected ${SHAPE}`);
      continue;
    }
    for (const [id, e] of Object.entries(entries)) {
      if (id in sources) { problems.push(`row ${id} is in both parity-status.d/${sources[id]} and parity-status.d/${name}; keep it in one file`); continue; }
      rows[id] = e;
      sources[id] = name;
    }
  }
  if (problems.length) throw new Error(problems.join('\n'));
  return { version: 1, rows, sources };
}

/** Validate the sidecar and every id it names. Throws with all problems listed. */
export function checkStatus(sidecar, rows) {
  const problems = [];
  const ids = new Set(rows.map((r) => r.id));
  const entries = sidecar && typeof sidecar === 'object' ? sidecar.rows : null;
  if (!entries || typeof entries !== 'object' || Array.isArray(entries)) {
    throw new Error(`parity status: expected ${SHAPE}`);
  }
  const where = (id) => (sidecar.sources?.[id] ? `parity-status.d/${sidecar.sources[id]}` : 'the parity status');
  for (const [id, e] of Object.entries(entries)) {
    if (!ids.has(id)) problems.push(`unknown row id in ${where(id)}: ${id}`);
    if (!e || typeof e !== 'object') { problems.push(`${id}: entry must be an object`); continue; }
    if (!STATUSES.includes(e.status)) problems.push(`${id}: status must be one of ${STATUSES.join('|')}, got ${JSON.stringify(e.status)}`);
    if (e.status === 'substitute' && !(typeof e.note === 'string' && e.note.trim())) problems.push(`${id}: substitute needs a note saying what replaces it`);
    // Hub #3104: a row that is partial or missing is a gap with an owner, never an unowned note.
    const owner = e.owner;
    const ownerOk = Array.isArray(owner) && owner.length > 0 && owner.every((o) => typeof o === 'string' && /^#[1-9][0-9]*$/.test(o));
    if (e.status === 'partial' || e.status === 'missing') {
      if (!ownerOk) problems.push(`${id}: a ${e.status} row needs "owner": ["#N", ...], the Issue(s) that will close the gap`);
      if (!(typeof e.note === 'string' && e.note.trim())) problems.push(`${id}: a ${e.status} row needs a note saying what is missing`);
    } else if (owner !== undefined) problems.push(`${id}: owner is only for partial or missing rows`);
    for (const k of Object.keys(e)) if (!['status', 'location', 'note', 'owner'].includes(k)) problems.push(`${id}: unknown field ${k}`);
  }
  if (problems.length) throw new Error(problems.join('\n'));
}

const cell = (s) => String(s ?? '').replace(/\|/g, '\\|').replace(/\r?\n/g, ' ').trim();
const code = (s) => (s ? `\`${String(s).replace(/`/g, "'").replace(/\|/g, '\\|')}\`` : '');

/** Merge status sidecar into rows: adds status/location/note (default missing). */
export function withStatus(rows, sidecar) {
  const entries = sidecar.rows;
  return rows.map((r) => {
    const e = entries[r.id];
    return { ...r, status: e?.status ?? 'missing', location: e?.location ?? '', note: e?.note ?? '', owner: e?.owner ?? [] };
  });
}

export function countByStatus(rows) {
  const counts = Object.fromEntries(STATUSES.map((s) => [s, 0]));
  for (const r of rows) counts[r.status] += 1;
  return counts;
}

/** Render PARITY.md from rows that already carry status. Deterministic. */
export function renderParity(rows) {
  const counts = countByStatus(rows);
  const lines = [];
  lines.push('# Desktop parity with the TUI');
  lines.push('');
  lines.push('<!-- GENERATED by `node scripts/parity.mjs` from docs/keybindings.md, the crates/rsi view modules and desktop/parity-status.d/*.json. Do not edit by hand: change the slice\'s parity-status.d file and regenerate. `node scripts/parity.mjs --check` (part of `npm test`) fails when this file is stale. -->');
  lines.push('');
  lines.push('The desktop is done when every row below is `done`, or a `substitute` that desktop/parity-audit.json justifies (the rsid operation, confirmation, refusal and interaction evidence). A `partial` or `missing` row is an honest gap and names the Issue that owns it. See the README section "Updating PARITY.md".');
  lines.push('');
  lines.push(`Rows: ${rows.length} total. ${STATUSES.map((s) => `${s} ${counts[s]}`).join(', ')}.`);
  lines.push('');
  const groups = [];
  for (const r of rows) {
    let g = groups.find((x) => x.name === r.group);
    if (!g) { g = { name: r.group, rows: [] }; groups.push(g); }
    g.rows.push(r);
  }
  for (const g of groups) {
    const gc = countByStatus(g.rows);
    lines.push(`## ${g.name}`);
    lines.push('');
    lines.push(`${g.rows.length} rows: ${STATUSES.map((s) => `${s} ${gc[s]}`).join(', ')}.`);
    lines.push('');
    lines.push('| id | kind | TUI binding / command | TUI module | status | desktop location | note |');
    lines.push('| --- | --- | --- | --- | --- | --- | --- |');
    for (const r of g.rows) {
      const binding = r.kind === 'overlay' || r.kind === 'overlay-ref' ? `${r.binding}${r.keys ? ` (${r.keys} keys)` : ''}` : r.binding;
      lines.push(`| ${code(r.id)} | ${r.kind} | ${r.kind === 'section' || r.kind.startsWith('overlay') ? cell(binding) : code(binding)} | ${code(r.module.replace(/^crates\/rsi\/src\//, ''))} | ${r.status} | ${code(r.location)} | ${cell(`${r.note || r.detail}${r.owner?.length ? ` (owner ${r.owner.join(', ')})` : ''}`)} |`);
    }
    lines.push('');
  }
  return `${lines.join('\n').replace(/\n+$/, '')}\n`;
}
