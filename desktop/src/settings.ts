// The settings pane (DOM side of settings-model.ts; slice 7, hub #3009): the
// TUI's category rail, section tabs, rows and info card. Daemon rows stage
// their edits; `w` (or Apply) sends one atomic UpdateDaemonConfig and re-reads
// the config; Esc or q closes and discards. Daemon text is set with
// textContent only, and the config the pane receives is already redacted.
import { openBudgetForm, openBudgets } from './budgets.js';
import { openTextBgEditor, setTextBg, textBg } from './textbg.js';
import { inTauri } from './api.js';
import { NOTIFY_OPEN_SETTINGS_BUTTON } from './notify-model.js';
import { notifyPermission, notifyPrefs, openNotificationSettings, refreshNotifyPermission, sendTestNotification, setNotifyPref } from './notify.js';
import { type PaneEffect, type PanelWidths, overlayKeyFor, panelColumns, parsePanels, resizePanels, rowClick, rowDetails, seatBudgets, SEAT_BUDGET_ROW, valueTooltip } from './settings-model.js';
import { askCloseChoice } from './settings-close.js';
import { dispatch } from './commands.js';
import { $, el, renderMessage, toast } from './dom.js';
import { SETTINGS_GROUPS, SETTINGS_SECTIONS } from './settings.generated.js';
import { settingsApi } from './settings-api.js';
import { syncListbox } from './listbox-a11y.js';
import {
  type Config, EMPTY_STAGING, INITIAL_NAV, type Match, type Nav, type PaneRow, REDACTED, type Staging, applyStaged,
  currentGroup, currentRow, currentSection, displayValue, goToSection, isDirty, isSecretField, isSteppable,
  nextMatch, optionText, paneKey, parseTextValue, pendingCount, prune, restartSuffix, rowValue, searchRows,
  sectionRows, sectionsOf, stage, stepValue, type Step,
} from './settings-model.js';
import { uiChanged } from './state.js';
import { openRoleEditor, openThemePicker, overrideCount, resetRoles, themeName } from './themes.js';
import { VIM_KEYS_TIP } from './keyengine.js';
import { currentPlatform, isVimKeysOn, setVimKeys } from './vimkeys.js';
import { rememberOpener, returnFocus } from './focus-return.js';
import { focusOwnsKey, paneKeyAllowed, paneKeyName } from './modal-gate.js';
import { isSettingsChord } from './registry.js';

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
  return sectionRows(currentSection(pane.nav).id, pane.config ?? {}, notifyPermission());
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

/** Read what macOS says about notifications (local #300) and redraw when it changed; read only, nothing is requested. */
async function refreshPermission(): Promise<void> {
  if (await refreshNotifyPermission() && pane.open) draw();
}

// Coming back from System Settings after turning notifications on or off: the line follows without reopening Settings.
if (typeof window !== 'undefined') window.addEventListener('focus', () => { if (pane.open) void refreshPermission(); });

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
    rememberOpener('settings');
    pane.open = true;
    pane.query = '';
    pane.matches = [];
    document.querySelector<HTMLElement>('.layout')!.hidden = true;
    view().hidden = false;
    void load(true);
    void refreshPermission();
  }
  if (section) pane.nav = goToSection(pane.nav, section);
  draw();
  view().focus({ preventScroll: true });
  uiChanged();
}

/**
 * Close the pane at once. Staged edits are discarded (the default) or, with `keepStaged`, kept for when the pane
 * opens again: another full-window view replacing Settings must not lose them. The Close button and Esc go
 * through requestCloseSettings, which asks first.
 */
export function closeSettings(opts: { keepStaged?: boolean } = {}): void {
  if (!pane.open) return;
  const n = settingsPending();
  if (!opts.keepStaged) pane.staging = EMPTY_STAGING;
  pane.open = false;
  view().hidden = true;
  document.querySelector<HTMLElement>('.layout')!.hidden = false;
  returnFocus('settings');
  saveNav();
  if (n > 0) {
    toast(opts.keepStaged
      ? `Settings closed; your ${n} unapplied change${n === 1 ? ' is' : 's are'} kept. Open Settings to apply or discard.`
      : `Closed settings; discarded ${n} pending setting${n === 1 ? '' : 's'}`);
  }
  uiChanged();
}

let askingClose = false;

/**
 * Close from the Close button, Esc or q (local #269). With nothing unapplied it just closes; otherwise it asks Apply,
 * Discard or Keep editing. Apply that the daemon refuses leaves the pane open with the changes still pending.
 */
export async function requestCloseSettings(): Promise<void> {
  if (!pane.open || askingClose) return;
  const n = settingsPending();
  if (n === 0) { closeSettings(); return; }
  askingClose = true;
  try {
    const choice = await askCloseChoice(n);
    if (choice === 'keep') { view().focus({ preventScroll: true }); return; }
    if (choice === 'apply') {
      await applyPending();
      if (settingsPending() > 0) { view().focus({ preventScroll: true }); return; }
    }
    closeSettings();
  } finally {
    askingClose = false;
  }
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
  if (row.widget.type === 'local' && (row.widget.local === 'notify-needs' || row.widget.local === 'notify-finished')) {
    const which = row.widget.local === 'notify-needs' ? 'needsMe' : 'finished';
    const on = step === 'activate' ? !notifyPrefs()[which] : step === 'next';
    setNotifyPref(which, on);
    toast(`${row.label}: ${on ? 'on' : 'off'}`, true);
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

/** Settings › Send a test notification: posts one now and says what happened. */
async function testNotification(): Promise<void> {
  try {
    await sendTestNotification();
    // The test may have just been answered at macOS's prompt: read the permission again and show a refusal at once.
    if (await refreshNotifyPermission()) draw();
    toast(inTauri() ? 'Test notification sent. If you do not see it, see System Settings › Notifications.' : 'Notifications are only available in the desktop app', inTauri());
  } catch (e) {
    toast(`Test notification failed: ${String(e)}`);
  }
}

/** Settings › Open Notification settings (shown only while macOS blocks notifications for RSI). */
async function openNotificationPane(): Promise<void> {
  try {
    await openNotificationSettings();
    toast('Opened System Settings › Notifications. Pick RSI and turn on Allow Notifications.', true);
  } catch (e) {
    toast(`Could not open System Settings: ${String(e)}`);
  }
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
      else if (w.local === 'notify-test') void testNotification();
      else if (w.local === 'notify-open-settings') void openNotificationPane();
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
  // Hub #3280: the mouse finishes what a click opened. Save writes now (the
  // Daemon Text Setting editor); Stage stages it for w, like Enter does.
  const actions = el('div', 'pick-actions');
  const cancel = el('button', 'btn', 'Cancel');
  cancel.type = 'button';
  const save = el('button', 'btn primary', editor ? 'Save' : 'Stage');
  save.type = 'submit';
  actions.append(cancel, save);
  cancel.addEventListener('click', () => d.close());
  form.append(input, err, hint, actions);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    if (save.disabled) return;
    const parsed = parseTextValue(row, input.value);
    if (!parsed.ok) { err.textContent = parsed.error; err.hidden = false; return; }
    if (editor) {
      save.disabled = true;
      void (async () => {
        try {
          await settingsApi.update([{ field, value: parsed.value }]);
          d.close();
          toast(`${row.label} → ${optionText(parsed.value) || editor.defaultLabel}${restartSuffix(field)}`, true);
          await load(true);
        } catch (e) {
          renderMessage(err, String(e));
          err.hidden = false;
        } finally {
          save.disabled = false;
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

/** Mark a node whose text names Vim keys, so the key-hints pass drops them with the switch off. */
function keyTip<T extends HTMLElement>(node: T): T {
  node.dataset.keyTip = '';
  return node;
}

function widgetNode(row: PaneRow, config: Config | null): HTMLElement {
  const w = row.widget;
  const box = el('span', `sv-value sv-${w.type}`);
  const hasOverlay = rowOverlays.has(row.id);
  const tip = valueTooltip(row, hasOverlay);
  if (tip) box.title = tip;
  const onOff = (on: boolean) => { box.textContent = on ? '● On' : '○ Off'; box.classList.toggle('on', on); };
  if (w.type === 'local') {
    if (w.local === 'vim-keys') onOff(isVimKeysOn());
    else if (w.local === 'theme') box.textContent = `${themeName()} ▾`;
    else if (w.local === 'roles') box.textContent = overrideCount() === 0 ? 'No overrides' : `${overrideCount()} override${overrideCount() === 1 ? '' : 's'}`;
    else if (w.local === 'text-bg') onOff(textBg().on);
    else if (w.local === 'notify-needs') onOff(notifyPrefs().needsMe);
    else if (w.local === 'notify-finished') onOff(notifyPrefs().finished);
    else if (w.local === 'notify-test') box.textContent = 'Send ▸';
    else if (w.local === 'notify-open-settings') box.textContent = NOTIFY_OPEN_SETTINGS_BUTTON;
    else if (w.local === 'budgets') { box.textContent = '▾ Enter lists · a adds'; box.dataset.keyHint = ''; }
    else if (w.local === 'provider-keys') { box.textContent = '▾ Enter opens · s r c d i'; box.dataset.keyHint = ''; }
    else if (w.local === 'text-bg-color') {
      const hex = textBg().hex;
      if (hex) { const sw = el('span', 'swatch'); sw.style.background = hex; box.append(sw); }
      box.append(document.createTextNode(hex ? ` ${hex}` : 'Theme default'));
    }
    else box.textContent = 'Reset';
    return box;
  }
  if (w.type === 'overlay') {
    const overlay = rowOverlays.get(row.id);
    box.textContent = overlay?.value?.() ?? 'Open ▸';
    return box;
  }
  if (w.type === 'tui-only') { box.textContent = 'TUI only'; box.title = w.why; return box; }
  if (w.type === 'partial' && hasOverlay) { box.textContent = 'Open ▸'; return box; }
  if (w.type === 'partial') { box.textContent = 'partial'; box.title = w.why; return box; }
  if (!config) { box.textContent = pane.loadError ? 'unavailable' : '…'; return box; }
  const v = rowValue(row, config, pane.staging);
  if (row.field && isSecretField(row.field)) { box.textContent = REDACTED; return box; }
  switch (w.type) {
    case 'switch': onOff(!!v); break;
    case 'choice': {
      if (v === undefined && row.id === 'model_control_mode') { box.textContent = 'unavailable'; break; }
      const arrow = (glyph: string, step: string) => {
        const a = el('span', 'sv-arrow', glyph);
        a.dataset.step = step;
        a.setAttribute('aria-hidden', 'true');
        return a;
      };
      box.append(arrow('◂', 'previous'), el('span', 'sv-choice-text', optionText(v)), arrow('▸', 'next'));
      break;
    }
    case 'text': case 'text-editor': {
      const shown = displayValue(v) || (w.type === 'text-editor' ? w.editor.defaultLabel : 'default');
      box.append(document.createTextNode(shown));
      // A visible pencil: this value is editable with one click.
      const pen = el('span', 'sv-edit', ' ✎');
      pen.setAttribute('aria-hidden', 'true');
      box.append(pen);
      break;
    }
    case 'display': {
      box.textContent = displayValue(v) || '—';
      if (hasOverlay) box.append(document.createTextNode(' ▸'));
      break;
    }
  }
  return box;
}

/** One entry per manager seat with its resolved coordinator budget in words; a sentence when there are no seats. */
function seatBudgetList(config: Config | null): HTMLElement {
  const seats = seatBudgets(config);
  if (seats.length === 0) {
    return el('p', 'meta settings-seats-empty', 'No manager seats are running, so there is no seat budget to show. Each running manager seat appears here with the point where it hands over.');
  }
  const list = el('ul', 'settings-seats');
  for (const s of seats) {
    const li = el('li', 'settings-seat');
    li.append(el('strong', '', `Seat ${s.seat} · ${s.model}`));
    for (const line of s.lines) li.append(el('div', '', line));
    list.append(li);
  }
  return list;
}

function infoCard(row: PaneRow | undefined, config: Config | null): HTMLElement {
  const card = el('aside', 'settings-info');
  card.dataset.keyTip = ''; // local #139: facts and summaries name letter keys; formatKeyTip drops them with the Vim keys off
  if (!row) { card.append(el('p', 'meta', 'This section has no rows.')); return card; }
  card.append(el('h3', '', row.label), el('p', '', row.summary));
  if (row.detail) card.append(el('p', 'meta', row.detail));
  if (row.id === SEAT_BUDGET_ROW) card.append(seatBudgetList(config));
  const facts = el('dl');
  const fact = (k: string, v: string, keyed = false) => {
    const dt = el('dt', '', k);
    if (keyed) dt.dataset.keyHint = ''; // a row key ("a" -> Add hook): the label empties with the Vim keys off and the CSS hides the pair
    facts.append(dt, el('dd', '', v));
  };
  const w = row.widget;
  const enter = w.type === 'switch' ? 'Flips the switch (staged; w applies)'
    : w.type === 'choice' ? 'Next choice (staged; + / - step, w applies)'
      : w.type === 'text' ? 'Type a value (staged; w applies)'
        : w.type === 'text-editor' ? 'Opens the text editor; Enter there saves at once'
          : w.type === 'local' ? (w.local === 'theme' ? 'Opens the theme picker' : w.local === 'roles' ? 'Opens the role editor' : w.local === 'reset-theme' ? 'Clears every role override' : w.local === 'text-bg' ? 'Turns the text-area background on or off' : w.local === 'text-bg-color' ? 'Opens the hex editor (#RRGGBB; Delete clears)' : w.local === 'budgets' ? 'Opens the budget policies (a adds, Enter edits, d deletes)' : w.local === 'provider-keys' ? 'Opens Provider keys (s sets, r rotates, c checks, d clears, i imports; values are never shown)' : VIM_KEYS_TIP)
            : w.type === 'display' ? 'Read-only' : w.type === 'tui-only' ? 'Nothing to change here: this is a TUI preference' : w.type === 'overlay' ? 'Opens the row\'s desktop view' : 'Not available';
  const overlay = rowOverlays.get(row.id);
  fact('What it does', overlay?.enter ?? enter);
  for (const [k, what] of Object.entries(overlay?.keys ?? {})) fact(k, what, true);
  fact('When it applies', row.apply);
  fact('Saved in', overlay?.stored ?? (row.id === 'model_control_mode' || row.field || w.type === 'display' ? 'the daemon' : w.type === 'local' ? 'this computer, for this app' : w.type === 'tui-only' ? "the TUI's own settings file (not the daemon)" : 'see the TUI'));
  if (w.type === 'choice') fact('Options', w.options.length > 12 ? `${w.options.slice(0, 6).join(', ')} … ${w.options.slice(-3).join(', ')}` : w.options.join(', '));
  if (w.type === 'tui-only') fact('Desktop', w.why);
  if (w.type === 'partial' && !overlay) fact('Desktop', `partial: ${w.why}`);
  card.append(facts);
  // The raw field names and values stay one click away, for someone matching them to the TUI or a bug report.
  const raw = rowDetails(row, config);
  if (raw.length > 0) {
    const more = el('details', 'settings-details');
    more.append(el('summary', '', 'Details'), el('pre', '', raw.join('\n')));
    card.append(more);
  }
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
  const config = pane.config;
  const pending = settingsPending();
  const head = root.querySelector<HTMLElement>('.settings-head-status')!;
  if (!pane.loading && !pane.busy && pane.loadError && !config) renderMessage(head, `Daemon config unavailable: ${pane.loadError}`);
  else {
    head.textContent = pane.loading ? 'Loading…'
      : pane.busy ? 'Applying…'
        : pending > 0 ? `${pending} pending change${pending === 1 ? '' : 's'} · Apply saves ${pending === 1 ? 'it' : 'them'} (or press w)` : 'No pending changes';
  }
  root.querySelector<HTMLButtonElement>('[data-settings="apply"]')!.disabled = pending === 0 || pane.busy;
  root.querySelector<HTMLButtonElement>('[data-settings="discard"]')!.disabled = pending === 0;

  const g = currentGroup(pane.nav);
  const rail = root.querySelector<HTMLElement>('.settings-rail')!;
  rail.classList.toggle('focused', pane.nav.focus === 'rail');
  rail.replaceChildren(...SETTINGS_GROUPS.map((grp, i) => {
    const b = el('button', `settings-group${grp.id === g.id ? ' sel' : ''}`);
    b.type = 'button';
    b.dataset.group = grp.id;
    b.setAttribute('role', 'option');
    b.setAttribute('aria-selected', String(grp.id === g.id));
    const sum = el('span', 'sg-summary', grp.summary);
    sum.dataset.keyTip = '';
    b.append(el('span', 'sg-label', grp.label), sum);
    b.addEventListener('click', () => { pane.nav = { ...pane.nav, group: i, focus: 'rail' }; draw(); });
    return b;
  }));
  // The pane holds focus and the arrows move the highlighted group: the rail names it while the rail has the keys (local #330).
  root.removeAttribute('aria-activedescendant');
  syncListbox(rail, pane.nav.focus === 'rail' ? root : rail, { label: 'Settings groups' });

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
    // A row is role=row in a grid (local #334): its cells keep any control inside reachable and named, which role=option would hide.
    // The current row is aria-selected and the pane's aria-activedescendant names it (syncListbox below).
    li.setAttribute('role', 'row');
    li.setAttribute('aria-selected', String(i === cur));
    li.dataset.index = String(i);
    const dirty = !!(config && row.field && isDirty(pane.staging, row.field, config[row.field]));
    const restart = row.apply.includes('restart');
    const mark = (cls: string, on: boolean, glyph: string, name: string) => {
      const c = el('span', cls, on ? glyph : '');
      c.setAttribute('role', 'gridcell');
      if (on) c.setAttribute('aria-label', name); else c.setAttribute('aria-hidden', 'true');
      return c;
    };
    const labelCell = el('span', 'sv-label', row.label);
    labelCell.setAttribute('role', 'rowheader');
    const valueCell = widgetNode(row, config);
    valueCell.setAttribute('role', 'gridcell');
    li.append(mark('sv-dirty', dirty, '●', 'Unsaved change'), labelCell, valueCell, mark('sv-apply', restart, '↻', 'Applies after a restart'));
    li.addEventListener('click', (ev) => {
      const t = ev.target as HTMLElement;
      if (t.closest('label')) return;
      // One click does it (local #269): it selects the row and starts what the row edits or opens. A switch or choice
      // changes only when its value is clicked (the right arrow, or the value, steps forward; the left arrow back).
      pane.nav = { ...pane.nav, rowOf: { ...pane.nav.rowOf, [section.id]: i }, focus: 'items' };
      const onValue = !!t.closest('.sv-value');
      if (rowClick(row, rowOverlays.has(row.id), onValue) === 'activate') {
        const back = t.closest<HTMLElement>('[data-step="previous"]');
        if (back) stepRow('previous');
        else activate();
      } else draw();
    });
    return li;
  }));
  list.children[cur]?.scrollIntoView({ block: 'nearest' });
  // The pane holds focus and the keys move the highlighted setting while the rows have them (local #334).
  syncListbox(list, pane.nav.focus === 'items' ? root : list, { label: 'Settings rows', role: 'grid' });
  root.querySelector('.settings-info')!.replaceWith(infoCard(rs[cur], config));
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
    el('h1', '', 'Settings'), keyTip(el('span', 'meta settings-head-status')), el('span', 'spacer'), search,
    btn('Apply (w)', 'apply', 'btn primary'), btn('Discard', 'discard'), btn('Refresh (R)', 'refresh'), btn('Close', 'close'),
  );
  const body = el('div', 'settings-body');
  const main = el('div', 'settings-main');
  const sectionSummary = el('div', 'settings-section-summary meta');
  sectionSummary.dataset.keyTip = '';
  const rowsList = el('ul', 'settings-rows');
  rowsList.setAttribute('aria-label', 'Settings rows');
  main.append(el('div', 'settings-tabs view-toggle'), sectionSummary, rowsList);
  const infoShell = el('aside', 'settings-info');
  infoShell.dataset.keyTip = '';
  body.append(el('nav', 'settings-rail'), main, infoShell);
  root.append(head, body);
  head.addEventListener('click', (ev) => {
    const b = (ev.target as HTMLElement).closest<HTMLButtonElement>('[data-settings]');
    if (!b) return;
    switch (b.dataset.settings) {
      case 'apply': dispatch('settings-apply', { source: 'button' }); break;
      case 'discard': dispatch('settings-discard', { source: 'button' }); break;
      case 'refresh': dispatch('settings-refresh', { source: 'button' }); break;
      case 'close': void requestCloseSettings(); break;
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
    const fx = paneKey(pane.nav, paneKeyName(ev), ev.shiftKey, rows().length);
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
      case 'close': void requestCloseSettings(); break;
      case 'reset-role': resetSelectedRole(); break;
      // A row overlay's own key (#88: a adds a hook or a provider) wins over the pane's list keys.
      case 'item': if (!overlayKey(fx)) listItem(fx.op); break;
      case 'provider-key': if (!overlayKey(fx)) providerKey(fx.op); break;
      case 'resize': resizePane(fx.op); break;
      case 'missing': if (!overlayKey(fx)) toast(`${fx.key} (${fx.what}) is not in the desktop yet`); break;
    }
  }, true);
  // Cmd-, (Ctrl-, off macOS) opens Settings from anywhere, with the Vim keys off too (local #269). A dialog that is
  // open keeps its focus, and the terminal overlay keeps its Ctrl chords.
  window.addEventListener('keydown', (ev) => {
    if (!isSettingsChord(ev, currentPlatform())) return;
    if (document.querySelector('dialog[open]')) return;
    if (ev.ctrlKey && (document.activeElement as HTMLElement | null)?.closest('.terminal-overlay')) return;
    ev.preventDefault();
    ev.stopPropagation();
    dispatch('settings-open', { source: 'key' });
  }, true);
}

/** Redraw after a theme change (the Theme & Colors rows show it). */
export function redrawSettings(): void {
  draw();
}
