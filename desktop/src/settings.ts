// The settings pane (DOM side of settings-model.ts; slice 7, hub #3009): the
// TUI's category rail, section tabs, rows and info card. Daemon rows stage
// their edits; `w` (or Apply) sends one atomic UpdateDaemonConfig and re-reads
// the config; Esc or q closes and discards. Daemon text is set with
// textContent only, and the config the pane receives is already redacted.
import { openBudgetForm, openBudgets } from './budgets.js';
import { openTextBgEditor, setTextBg, textBg } from './textbg.js';
import { type PaneEffect, type PanelWidths, overlayKeyFor, panelColumns, parsePanels, resizePanels } from './settings-model.js';
import { dispatch } from './commands.js';
import { $, el, toast } from './dom.js';
import { SETTINGS_GROUPS, SETTINGS_SECTIONS } from './settings.generated.js';
import { settingsApi } from './settings-api.js';
import {
  type Config, EMPTY_STAGING, INITIAL_NAV, type Match, type Nav, type PaneRow, REDACTED, type Staging, applyStaged,
  currentGroup, currentRow, currentSection, displayValue, goToSection, isDirty, isSecretField, isSteppable,
  nextMatch, optionText, paneKey, parseTextValue, pendingCount, prune, restartSuffix, rowValue, searchRows,
  sectionRows, sectionsOf, stage, stepValue, type Step,
} from './settings-model.js';
import { uiChanged } from './state.js';
import { openRoleEditor, openThemePicker, overrideCount, resetRoles, themeName } from './themes.js';
import { isVimKeysOn, setVimKeys } from './vimkeys.js';
import { focusOwnsKey, paneKeyAllowed } from './modal-gate.js';

interface Pane {
  open: boolean;
  nav: Nav;
  config: Config | null;
  loadError: string | null;
  loading: boolean;
  staging: Staging;
  query: string;
  matches: Match[];
  busy: boolean;
}

const pane: Pane = {
  open: false, nav: INITIAL_NAV, config: null, loadError: null, loading: false,
  staging: EMPTY_STAGING, query: '', matches: [], busy: false,
};

const LS_NAV = 'rsi.desktop.settings.nav.v1';
let vimToggle: HTMLElement | null = null;

/**
 * A row another slice serves with its own overlay (local #88: Claude hooks,
 * API providers, the bridges and the model-role routes). Enter runs it with
 * `'Enter'`; a TUI list key in `keys` (a add, d delete, ...) runs it with that
 * key instead of the "not in the desktop yet" notice.
 */
export interface RowOverlay {
  /** What Enter does (the info card). */
  enter: string;
  /** Where the row's data lives (the info card). */
  stored: string;
  /** Extra TUI keys on the row and what they do. */
  keys?: Readonly<Record<string, string>>;
  run(key: string): void;
  /** The row's live value once loaded (a status line); null while it is not. */
  value?(): string | null;
  /** Reload `value()` from the daemon (the pane opening, R); redraws when it settles. */
  refresh?(): Promise<void>;
}

const rowOverlays = new Map<string, RowOverlay>();

/** Serve the settings row `rowId` with a slice overlay (registered from the slice's init). */
export function registerRowOverlay(rowId: string, overlay: RowOverlay): void {
  rowOverlays.set(rowId, overlay);
}

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

export function isSettingsOpen(): boolean {
  return pane.open;
}

export function settingsPending(): number {
  return pane.config ? pendingCount(pane.staging, pane.config) : 0;
}

function view(): HTMLElement {
  return $('settings-view');
}

function rows(): PaneRow[] {
  return sectionRows(currentSection(pane.nav).id, pane.config ?? {});
}

function selectedRow(): PaneRow | undefined {
  const r = rows();
  return r[currentRow(pane.nav, r.length)];
}

// ---------- loading and applying ----------

async function load(quiet = false): Promise<void> {
  pane.loading = true;
  draw();
  try {
    const config = await settingsApi.getConfig();
    pane.config = config;
    pane.loadError = null;
    pane.staging = prune(pane.staging, config);
    if (!quiet) toast('Settings refreshed from the daemon', true);
  } catch (e) {
    pane.loadError = String(e);
    if (!quiet) toast(`Could not read the daemon config: ${String(e)}`);
  } finally {
    pane.loading = false;
    draw();
    uiChanged();
  }
  void refreshOverlays();
}

/** Reload every daemon-state row's value (storage, pools, cold archive, usage); a failure shows on its row. */
async function refreshOverlays(): Promise<void> {
  await Promise.all([...rowOverlays.values()].map((o) => o.refresh?.().catch(() => undefined)));
  draw();
}

export async function applyPending(): Promise<void> {
  if (!pane.config) { toast('The daemon config is not loaded (R retries)'); return; }
  if (pane.busy) return;
  pane.busy = true;
  draw();
  try {
    const out = await applyStaged(settingsApi, pane.staging, pane.config);
    pane.config = out.config;
    pane.staging = out.staging;
    toast(out.message, out.error === null && out.applied.length > 0);
  } finally {
    pane.busy = false;
    draw();
    uiChanged();
  }
}

export function discardPending(): void {
  const n = settingsPending();
  pane.staging = EMPTY_STAGING;
  if (n > 0) toast(`Discarded ${n} pending setting${n === 1 ? '' : 's'}`, true);
  draw();
  uiChanged();
}

export function refreshSettings(): void {
  void load(false);
}

// ---------- open / close ----------

export function openSettings(section?: string): void {
  if (!pane.open) {
    // The pane replaces the session layout the way the Issues workspace does.
    if (!$('issues-view').hidden) dispatch('issues-close', { source: 'button' });
    pane.open = true;
    pane.query = '';
    pane.matches = [];
    document.querySelector<HTMLElement>('.layout')!.hidden = true;
    view().hidden = false;
    void load(true);
  }
  if (section) pane.nav = goToSection(pane.nav, section);
  draw();
  view().focus({ preventScroll: true });
  uiChanged();
}

/** Close the pane; staged edits are discarded (the TUI's Esc / q). */
export function closeSettings(): void {
  if (!pane.open) return;
  const n = settingsPending();
  pane.staging = EMPTY_STAGING;
  pane.open = false;
  view().hidden = true;
  document.querySelector<HTMLElement>('.layout')!.hidden = false;
  saveNav();
  if (n > 0) toast(`Closed settings; discarded ${n} pending setting${n === 1 ? '' : 's'}`);
  uiChanged();
}

function saveNav(): void {
  try { storage()?.setItem(LS_NAV, JSON.stringify(pane.nav)); } catch { /* ignore */ }
}

function loadNav(): Nav {
  try {
    const v = JSON.parse(storage()?.getItem(LS_NAV) ?? 'null') as Partial<Nav> | null;
    if (!v || typeof v !== 'object') return INITIAL_NAV;
    const group = Number.isInteger(v.group) ? Math.max(0, Math.min(SETTINGS_GROUPS.length - 1, v.group!)) : 0;
    const sectionOf: Record<string, string> = {};
    for (const [g, s] of Object.entries(v.sectionOf ?? {})) if (SETTINGS_SECTIONS.some((x) => x.id === s && x.group === g)) sectionOf[g] = s;
    const rowOf: Record<string, number> = {};
    for (const [s, r] of Object.entries(v.rowOf ?? {})) if (Number.isInteger(r) && r >= 0) rowOf[s] = r;
    return { group, sectionOf, rowOf, focus: v.focus === 'items' ? 'items' : 'rail' };
  } catch {
    return INITIAL_NAV;
  }
}

// ---------- editing ----------

function needConfig(): Config | null {
  if (!pane.config) {
    toast(pane.loadError ? `The daemon config is not loaded: ${pane.loadError} (R retries)` : 'The daemon config is still loading');
    return null;
  }
  return pane.config;
}

function stepRow(step: Step): void {
  const row = selectedRow();
  if (!row) return;
  if (row.widget.type === 'local' && row.widget.local === 'text-bg') {
    const on = step === 'activate' ? !textBg().on : step === 'next';
    setTextBg({ ...textBg(), on });
    toast(`Text area background ${on ? 'on' : 'off'}`, true);
    draw();
    return;
  }
  if (row.widget.type === 'local' && row.widget.local === 'vim-keys') {
    const on = step === 'activate' ? !isVimKeysOn() : step === 'next';
    setVimKeys(on);
    toast(`Vim keys ${on ? 'on' : 'off'}`, true);
    uiChanged();
    draw();
    return;
  }
  if (!isSteppable(row) || !row.field) {
    if (step === 'activate') activate();
    return;
  }
  const config = needConfig();
  if (!config) return;
  const live = config[row.field];
  const next = stepValue(row, rowValue(row, config, pane.staging), step);
  if (next === undefined) return;
  pane.staging = stage(pane.staging, row.field, live, next);
  draw();
  uiChanged();
}

function activate(): void {
  const row = selectedRow();
  if (!row) return;
  const overlay = rowOverlays.get(row.id);
  if (overlay) { overlay.run('Enter'); return; }
  const w = row.widget;
  switch (w.type) {
    case 'switch': case 'choice': stepRow('activate'); return;
    case 'local':
      if (w.local === 'theme') openThemePicker();
      else if (w.local === 'roles') openRoleEditor();
      else if (w.local === 'reset-theme') resetRoles();
      else if (w.local === 'text-bg-color') openTextBgEditor(() => draw());
      else if (w.local === 'budgets') openBudgets(() => draw());
      else if (w.local === 'provider-keys') dispatch('admin-provider-keys', { source: 'key' });
      else stepRow('activate');
      return;
    case 'text': case 'text-editor': editText(row); return;
    case 'display': toast(`${row.label} is read-only`); return;
    case 'overlay': toast(`${row.label}: its desktop view is not available`); return;
    case 'tui-only': case 'partial': toast(`${row.label}: ${w.why}`); return;
  }
}

/** A text row: a staged value (text) or the TUI's Daemon Text Setting editor (writes on Enter). */
function editText(row: PaneRow): void {
  const config = needConfig();
  if (!config || !row.field) return;
  const field = row.field;
  const editor = row.widget.type === 'text-editor' ? row.widget.editor : null;
  let dlg = document.getElementById('settings-text') as HTMLDialogElement | null;
  if (!dlg) {
    dlg = document.createElement('dialog');
    dlg.id = 'settings-text';
    dlg.className = 'pick';
    document.body.append(dlg);
  }
  const d = dlg;
  const form = el('form');
  form.method = 'dialog';
  form.append(el('h2', '', editor?.title ?? row.label));
  if (editor) form.append(el('div', 'pick-empty', editor.prompt));
  const input = el('input');
  input.type = 'text';
  input.dataset.vim = 'on';
  input.spellcheck = false;
  const cur = rowValue(row, config, pane.staging);
  input.value = displayValue(cur);
  input.placeholder = editor?.defaultLabel ?? 'empty for the default';
  const err = el('div', 'form-error');
  err.hidden = true;
  const hint = el('div', 'pick-empty', editor
    ? `${editor.hint}; Enter saves now (empty for the ${editor.defaultLabel}), Esc cancels`
    : 'Enter stages the value (w applies), Esc cancels');
  form.append(input, err, hint);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    const parsed = parseTextValue(row, input.value);
    if (!parsed.ok) { err.textContent = parsed.error; err.hidden = false; return; }
    if (editor) {
      void (async () => {
        try {
          await settingsApi.update([{ field, value: parsed.value }]);
          d.close();
          toast(`${row.label} → ${optionText(parsed.value) || editor.defaultLabel}${restartSuffix(field)}`, true);
          await load(true);
        } catch (e) {
          err.textContent = String(e);
          err.hidden = false;
        }
      })();
      return;
    }
    pane.staging = stage(pane.staging, field, config[field], parsed.value);
    d.close();
    draw();
    uiChanged();
  });
  d.replaceChildren(form);
  d.onkeydown = (ev) => { if (focusOwnsKey(ev.key, ev.target as Element | null)) return; if (ev.key === 'Escape') { ev.stopPropagation(); } };
  d.showModal();
  input.focus();
  input.select();
}

function resetSelectedRole(): void {
  const row = selectedRow();
  if (row?.widget.type === 'local' && row.widget.local === 'roles') openRoleEditor();
  else toast('Delete resets a theme role: open Theme roles (Enter) and press Delete on the role');
}

// ---------- search ----------

function startSearch(): void {
  const input = view().querySelector<HTMLInputElement>('.settings-search');
  if (!input) return;
  input.hidden = false;
  input.value = pane.query;
  input.focus();
  input.select();
}

function jumpTo(m: Match): void {
  pane.nav = goToSection(pane.nav, m.section, m.row);
}

function runSearch(query: string): void {
  pane.query = query.trim();
  pane.matches = searchRows(pane.query, pane.config ?? {});
  const at = { section: currentSection(pane.nav).id, row: currentRow(pane.nav, rows().length) - 1 };
  const hit = nextMatch(pane.matches, at, 1);
  if (!hit) { toast(pane.query ? `No setting matches “${pane.query}”` : 'Search cleared'); draw(); return; }
  jumpTo(hit.match);
  draw();
}

function nextHit(dir: 1 | -1): void {
  if (pane.matches.length === 0) { toast(pane.query ? `No setting matches “${pane.query}”` : 'No search (press / first)'); return; }
  const at = { section: currentSection(pane.nav).id, row: currentRow(pane.nav, rows().length) };
  const hit = nextMatch(pane.matches, at, dir);
  if (!hit) return;
  if (hit.wrapped) toast(dir > 0 ? 'Search wrapped to the first match' : 'Search wrapped to the last match');
  jumpTo(hit.match);
  draw();
}

// ---------- rendering ----------

function widgetNode(row: PaneRow, config: Config | null): HTMLElement {
  const w = row.widget;
  const box = el('span', `sv-value sv-${w.type}`);
  if (w.type === 'local') {
    if (w.local === 'vim-keys' && vimToggle) { box.append(vimToggle); return box; }
    if (w.local === 'theme') box.textContent = `▾ ${themeName()}`;
    else if (w.local === 'roles') box.textContent = `${overrideCount()} override${overrideCount() === 1 ? '' : 's'} · Enter edits`;
    else if (w.local === 'text-bg') box.textContent = textBg().on ? '━━● ON' : '○── OFF';
    else if (w.local === 'budgets') { box.textContent = '▾ Enter lists · a adds'; box.dataset.keyHint = ''; }
    else if (w.local === 'provider-keys') { box.textContent = '▾ Enter opens · s r c d i'; box.dataset.keyHint = ''; }
    else if (w.local === 'text-bg-color') {
      const hex = textBg().hex;
      if (hex) { const sw = el('span', 'swatch'); sw.style.background = hex; box.append(sw); }
      box.append(document.createTextNode(hex ? ` ${hex}` : 'theme default · Enter edits'));
    }
    else box.textContent = '[ Reset ]';
    return box;
  }
  if (w.type === 'overlay') {
    const overlay = rowOverlays.get(row.id);
    box.textContent = overlay?.value?.() ?? 'Enter opens ▸';
    return box;
  }
  if (w.type === 'tui-only') { box.textContent = 'TUI only'; box.title = w.why; return box; }
  if (w.type === 'partial' && rowOverlays.has(row.id)) { box.textContent = 'Enter opens ▸'; return box; }
  if (w.type === 'partial') { box.textContent = 'partial'; box.title = w.why; return box; }
  if (!config) { box.textContent = pane.loadError ? 'unavailable' : '…'; return box; }
  const v = rowValue(row, config, pane.staging);
  if (row.field && isSecretField(row.field)) { box.textContent = REDACTED; return box; }
  switch (w.type) {
    case 'switch': box.textContent = v ? '━━● ON' : '○── OFF'; box.classList.toggle('on', !!v); break;
    case 'choice': box.textContent = v === undefined && row.id === 'model_control_mode' ? 'unavailable' : `◂ ${optionText(v)} ▸`; break;
    case 'text': case 'text-editor': box.textContent = displayValue(v) || (w.type === 'text-editor' ? w.editor.defaultLabel : 'default'); break;
    case 'display': box.textContent = displayValue(v) || '—'; break;
  }
  return box;
}

function infoCard(row: PaneRow | undefined): HTMLElement {
  const card = el('aside', 'settings-info');
  if (!row) { card.append(el('p', 'meta', 'This section has no rows.')); return card; }
  card.append(el('h3', '', row.label), el('p', '', row.summary));
  if (row.detail) card.append(el('p', 'meta', row.detail));
  const facts = el('dl');
  const fact = (k: string, v: string) => { facts.append(el('dt', '', k), el('dd', '', v)); };
  const w = row.widget;
  const enter = w.type === 'switch' ? 'Flips the switch (staged; w applies)'
    : w.type === 'choice' ? 'Next choice (staged; + / - step, w applies)'
      : w.type === 'text' ? 'Type a value (staged; w applies)'
        : w.type === 'text-editor' ? 'Opens the text editor; Enter there saves at once'
          : w.type === 'local' ? (w.local === 'theme' ? 'Opens the theme picker' : w.local === 'roles' ? 'Opens the role editor' : w.local === 'reset-theme' ? 'Clears every role override' : w.local === 'text-bg' ? 'Turns the text-area background on or off' : w.local === 'text-bg-color' ? 'Opens the hex editor (#RRGGBB; Delete clears)' : w.local === 'budgets' ? 'Opens the budget policies (a adds, Enter edits, d deletes)' : w.local === 'provider-keys' ? 'Opens Provider keys (s sets, r rotates, c checks, d clears, i imports; values are never shown)' : 'Turns the Vim keys on or off')
            : w.type === 'display' ? 'Read-only' : w.type === 'tui-only' ? 'Nothing to change here: this is a TUI preference' : w.type === 'overlay' ? 'Opens the row\'s desktop view' : 'Not available';
  const overlay = rowOverlays.get(row.id);
  fact('Enter', overlay?.enter ?? enter);
  for (const [k, what] of Object.entries(overlay?.keys ?? {})) fact(k, what);
  fact('Applies', row.apply);
  fact('Stored', overlay?.stored ?? (row.id === 'model_control_mode' ? 'the daemon (UpdateModelControlPolicy)' : row.field ? `daemon field ${row.field}` : w.type === 'local' ? 'this desktop (localStorage)' : w.type === 'tui-only' ? "the TUI's state.json (not rsid)" : 'see the TUI'));
  if (w.type === 'choice') fact('Options', w.options.length > 12 ? `${w.options.slice(0, 6).join(', ')} … ${w.options.slice(-3).join(', ')}` : w.options.join(', '));
  if (w.type === 'tui-only') fact('Desktop', w.why);
  if (w.type === 'partial' && !overlay) fact('Desktop', `partial: ${w.why}`);
  card.append(facts);
  return card;
}

/** Which settings control holds focus, by a key that survives a redraw. */
function focusedControl(root: HTMLElement): string | null {
  const a = document.activeElement as HTMLElement | null;
  if (!a || !root.contains(a) || a.tagName !== 'BUTTON') return null;
  if (a.dataset.settings) return `[data-settings="${a.dataset.settings}"]`;
  if (a.dataset.group) return `.settings-rail [data-group="${a.dataset.group}"]`;
  if (a.dataset.section) return `.settings-tabs [data-section="${a.dataset.section}"]`;
  return null;
}

/** Put focus back on the rebuilt control; a header button that became disabled hands it to Refresh. */
function restoreFocus(root: HTMLElement, selector: string | null): void {
  if (!selector) return;
  let b = root.querySelector<HTMLButtonElement>(selector);
  if (!b || b.disabled) b = root.querySelector<HTMLButtonElement>('[data-settings="refresh"]');
  if (b && document.activeElement !== b) b.focus({ preventScroll: true });
}

function draw(): void {
  if (!pane.open) return;
  const root = view();
  // Hub #3130: a clicked header, rail or tab button keeps keyboard focus across a
  // redraw, so the next Enter/Space activates that control (the activation guard
  // keeps it from the pane), never the selected row.
  const keep = focusedControl(root);
  // Park the Vim keys toggle first, so it stays in the document (vimkeys.ts
  // finds it by id) whichever section is drawn.
  if (vimToggle) root.querySelector('.settings-parking')?.append(vimToggle);
  const config = pane.config;
  const pending = settingsPending();
  const head = root.querySelector<HTMLElement>('.settings-head-status')!;
  head.textContent = pane.loading ? 'Loading…'
    : pane.busy ? 'Applying…'
      : pane.loadError && !config ? `Daemon config unavailable: ${pane.loadError}`
        : pending > 0 ? `${pending} pending · w applies · Esc discards` : 'No pending changes';
  root.querySelector<HTMLButtonElement>('[data-settings="apply"]')!.disabled = pending === 0 || pane.busy;
  root.querySelector<HTMLButtonElement>('[data-settings="discard"]')!.disabled = pending === 0;

  const g = currentGroup(pane.nav);
  const rail = root.querySelector<HTMLElement>('.settings-rail')!;
  rail.classList.toggle('focused', pane.nav.focus === 'rail');
  rail.replaceChildren(...SETTINGS_GROUPS.map((grp, i) => {
    const b = el('button', `settings-group${grp.id === g.id ? ' sel' : ''}`);
    b.type = 'button';
    b.dataset.group = grp.id;
    b.append(el('span', 'sg-label', grp.label), el('span', 'sg-summary', grp.summary));
    b.addEventListener('click', () => { pane.nav = { ...pane.nav, group: i, focus: 'rail' }; draw(); });
    return b;
  }));

  const section = currentSection(pane.nav);
  const tabs = root.querySelector<HTMLElement>('.settings-tabs')!;
  tabs.replaceChildren(...sectionsOf(g.id).map((s) => {
    const t = el('button', `seg${s.id === section.id ? ' active' : ''}`, s.label);
    t.type = 'button';
    t.dataset.section = s.id;
    t.addEventListener('click', () => { pane.nav = goToSection(pane.nav, s.id, pane.nav.rowOf[s.id] ?? 0); draw(); });
    return t;
  }));
  root.querySelector<HTMLElement>('.settings-section-summary')!.textContent = section.summary;
  restoreFocus(root, keep);

  const list = root.querySelector<HTMLElement>('.settings-rows')!;
  list.classList.toggle('focused', pane.nav.focus === 'items');
  const rs = rows();
  const cur = currentRow(pane.nav, rs.length);
  const hits = new Set(pane.matches.filter((m) => m.section === section.id).map((m) => m.row));
  list.replaceChildren(...rs.map((row, i) => {
    const li = el('li', `settings-row${i === cur ? ' sel' : ''}${row.widget.type === 'partial' && !rowOverlays.has(row.id) ? ' partial' : ''}${row.widget.type === 'tui-only' ? ' tui-only' : ''}${hits.has(i) ? ' hit' : ''}`);
    const dirty = !!(config && row.field && isDirty(pane.staging, row.field, config[row.field]));
    li.append(el('span', 'sv-dirty', dirty ? '●' : ''), el('span', 'sv-label', row.label), widgetNode(row, config));
    li.append(el('span', 'sv-apply', row.apply.includes('restart') ? '↻' : ''));
    li.addEventListener('click', (ev) => {
      if ((ev.target as HTMLElement).closest('label')) return;
      const again = i === cur && pane.nav.focus === 'items';
      pane.nav = { ...pane.nav, rowOf: { ...pane.nav.rowOf, [section.id]: i }, focus: 'items' };
      if (again && (ev.target as HTMLElement).closest('.sv-value')) activate();
      else draw();
    });
    return li;
  }));
  list.children[cur]?.scrollIntoView({ block: 'nearest' });
  root.querySelector('.settings-info')!.replaceWith(infoCard(rs[cur]));
}

// ---------- wiring ----------

function isTyping(t: HTMLElement | null): boolean {
  return !!t?.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]');
}

function build(): HTMLElement {
  const root = el('section', 'settings-pane');
  root.id = 'settings-view';
  root.hidden = true;
  root.tabIndex = -1;
  root.setAttribute('aria-label', 'Settings');
  const head = el('header', 'settings-head');
  const btn = (text: string, key: string, cls = 'btn') => {
    const b = el('button', cls, text);
    b.type = 'button';
    b.dataset.settings = key;
    return b;
  };
  const search = el('input', 'settings-search');
  search.type = 'search';
  search.placeholder = 'Search settings… (Enter jumps, n/N next)';
  search.setAttribute('aria-label', 'Search settings');
  search.hidden = true;
  head.append(
    el('h1', '', 'Settings'), el('span', 'meta settings-head-status'), el('span', 'spacer'), search,
    btn('Apply (w)', 'apply', 'btn primary'), btn('Discard', 'discard'), btn('Refresh (R)', 'refresh'), btn('Close', 'close'),
  );
  const body = el('div', 'settings-body');
  const main = el('div', 'settings-main');
  const rowsList = el('ul', 'settings-rows');
  rowsList.setAttribute('aria-label', 'Settings rows');
  main.append(el('div', 'settings-tabs view-toggle'), el('div', 'settings-section-summary meta'), rowsList);
  body.append(el('nav', 'settings-rail'), main, el('aside', 'settings-info'));
  const parking = el('div', 'settings-parking');
  parking.hidden = true;
  root.append(head, body, parking);
  head.addEventListener('click', (ev) => {
    const b = (ev.target as HTMLElement).closest<HTMLButtonElement>('[data-settings]');
    if (!b) return;
    switch (b.dataset.settings) {
      case 'apply': dispatch('settings-apply', { source: 'button' }); break;
      case 'discard': dispatch('settings-discard', { source: 'button' }); break;
      case 'refresh': dispatch('settings-refresh', { source: 'button' }); break;
      case 'close': closeSettings(); break;
    }
  });
  search.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Enter') { ev.preventDefault(); runSearch(search.value); search.hidden = !pane.query; root.focus(); }
    else if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); search.value = ''; pane.query = ''; pane.matches = []; search.hidden = true; root.focus(); draw(); }
  });
  return root;
}

// ---------- gap sweep G4: list rows, provider keys, panel widths ----------

const PANELS_KEY = 'rsi.desktop.settingsPanels';
let panels: PanelWidths = { rail: null, info: null };

function applyPanels(): void {
  const body = view().querySelector<HTMLElement>('.settings-body');
  if (body) body.style.gridTemplateColumns = panelColumns(panels);
}

function resizePane(op: '>' | '<' | '='): void {
  panels = resizePanels(panels, pane.nav.focus === 'rail' ? 'rail' : 'items', op);
  try { storage()?.setItem(PANELS_KEY, JSON.stringify(panels)); } catch { /* this run only */ }
  applyPanels();
}

/** `a` / `d`: add to or delete from the selected list (Budgets; the others are TUI-file lists). */
/** Run the effect's key through the selected row's overlay when it names that key (overlayKeyFor). */
function overlayKey(fx: PaneEffect): boolean {
  const overlay = pane.nav.focus === 'items' ? rowOverlays.get(selectedRow()?.id ?? '') : undefined;
  const key = overlayKeyFor(fx, overlay?.keys);
  if (key === null || !overlay) return false;
  overlay.run(key);
  return true;
}

function listItem(op: 'add' | 'delete'): void {
  const row = selectedRow();
  const w = row?.widget;
  if (w?.type === 'local' && w.local === 'budgets') {
    if (op === 'add') openBudgetForm(null, () => draw());
    else openBudgets(() => draw());
    return;
  }
  if (w?.type === 'local' && w.local === 'provider-keys') { providerKey('d'); return; }
  toast(op === 'add' ? 'a adds to a list row (Budgets); this row is not a list' : 'd deletes from a list row; open the list with Enter', true);
}

/** `s` / `r` / `c` / `i` (and `d`) on Provider Keys: the admin overlay runs them on the slot you pick. */
function providerKey(op: 's' | 'r' | 'c' | 'i' | 'd'): void {
  const w = selectedRow()?.widget;
  if (!(w?.type === 'local' && w.local === 'provider-keys')) {
    toast(`${op} acts on the Provider Keys row (Safety & Spend › Provider Keys)`, true);
    return;
  }
  dispatch('admin-provider-keys', { source: 'key' });
}

export function initSettings(): void {
  const root = build();
  panels = parsePanels(storage()?.getItem(PANELS_KEY) ?? null);
  // The pane takes the place of the session layout, like the Issues view.
  const layout = document.querySelector('.layout');
  if (layout) layout.after(root);
  else document.body.append(root);
  // After the mount: view() looks the pane up by id, and before this it threw "missing
  // #settings-view" out of initSlices, so the app never finished booting.
  applyPanels();
  // The Vim keys toggle moves from the top bar into Settings › Workspace › Editing Mode.
  vimToggle = document.getElementById('vim-keys')?.closest('label') ?? null;
  if (vimToggle) root.querySelector('.settings-parking')!.append(vimToggle);
  pane.nav = loadNav();
  // Capture phase, registered before the other panes: the settings pane takes its keys first.
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (!pane.open || ev.isComposing) return;
    if (!paneKeyAllowed(isVimKeysOn(), ev)) return;
    if (document.querySelector('dialog[open]')) return;
    const target = document.activeElement as HTMLElement | null;
    if (ev.ctrlKey || ev.metaKey || ev.altKey || isTyping(target)) return;
    if (ev.key === 'Enter' && target?.closest('button')) return;
    const fx = paneKey(pane.nav, ev.key, ev.shiftKey, rows().length);
    if (fx.kind === 'none') return;
    ev.preventDefault();
    ev.stopImmediatePropagation();
    switch (fx.kind) {
      case 'nav': pane.nav = fx.nav; draw(); break;
      case 'step': stepRow(fx.step); break;
      case 'activate': activate(); break;
      case 'apply': dispatch('settings-apply', { source: 'key' }); break;
      case 'refresh': dispatch('settings-refresh', { source: 'key' }); break;
      case 'search': startSearch(); break;
      case 'match': nextHit(fx.dir); break;
      case 'close': closeSettings(); break;
      case 'reset-role': resetSelectedRole(); break;
      // A row overlay's own key (#88: a adds a hook or a provider) wins over the pane's list keys.
      case 'item': if (!overlayKey(fx)) listItem(fx.op); break;
      case 'provider-key': if (!overlayKey(fx)) providerKey(fx.op); break;
      case 'resize': resizePane(fx.op); break;
      case 'missing': if (!overlayKey(fx)) toast(`${fx.key} (${fx.what}) is not in the desktop yet`); break;
    }
  }, true);
}

/** Redraw after a theme change (the Theme & Colors rows show it). */
export function redrawSettings(): void {
  draw();
}
