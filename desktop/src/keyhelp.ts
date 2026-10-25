// The keybindings help overlay (DOM side of keyhelp-model.ts; slice 7). It is
// built from the snapshot captured when it opens, so the Contextual view lists
// what could run where the operator was. Closing it returns focus there.
import { el } from './dom.js';
import {
  HELP_VIEWS, type HelpRow, type HelpState, VIEW_TITLES, allOverlayRows, allRows, contextualRows, filterRows, helpFooter, helpKey,
  gettingStartedRows, initialHelp, overlayRows, plainRows, symbolRows,
} from './keyhelp-model.js';
import type { UiSnapshot } from './registry.js';
import { hintLabel } from './keylabel.js';
import { currentPlatform } from './vimkeys.js';
import { KEY_TABLE_ATTR } from './keytable.js';
import { focusOwnsKey, paneKeyName, tabIsNative } from './modal-gate.js';

const ROW_PX = 22;
const SEEN_KEY = 'rsi.desktop.help.seen';

/** Exact overlay names, else names matched case-insensitively (a dialog title). */
function matchOverlay(names: readonly string[]): string[] {
  const all = allOverlayRows().map((r) => r.section.replace(/^Overlay: /, ''));
  const known = [...new Set(all)];
  return names.flatMap((n) => known.filter((k) => k === n || k.toLowerCase() === n.toLowerCase()));
}

export function isKeyHelpOpen(): boolean {
  return (document.getElementById('keys-help') as HTMLDialogElement | null)?.open ?? false;
}

/**
 * The overlay tables for where help was opened from (gap sweep G4): the open
 * dialog or the files panel that has the keys, by the TUI overlay it mirrors.
 */
const OVERLAY_OF: ReadonlyArray<readonly [string, readonly string[]]> = [
  ['#launch-dialog', ['New Session Prompt', 'New Session Settings']],
  ['#continue-dialog', ['Continue Prompt']],
  ['#g1-modal-dialog', ['Input Modal']],
  ['.model-dropdown', ['Model Picker']],
  ['.entity-dialog', ['Create Entity / Normal', 'Create Entity / Insert', 'Create Entity / Body', 'Create Entity / Topology']],
  ['#admin-schedules-dialog', ['Scheduled Jobs Browser']],
  ['#notice-dialog', ['Notifications']],
  ['#recent-dialog', ['Recent Completions']],
  ['#trash-dialog', ['Trash']],
  ['#question-dialog', ['Question / Normal', 'Question / Insert']],
  ['.text-bg-dialog', ['Text Area Background']],
  ['#palette', ['Command Palette']],
  ['.files-picker', ['Telescope']],
  ['.files-viewer', ['File Viewer']],
  ['.files-drawer', ['File Explorer', 'File Explorer / Finder', 'File Explorer / Viewer Focus']],
  ['.files-overlay', ['File Explorer']],
];

/** The key-table names the ? help lists for where it was opened (exported for tests). */
export function overlayNames(origin: Element | null): string[] {
  if (!origin) return [];
  // A dialog that names its own table (data-key-table) wins over the selector list and the title.
  const named = origin.closest(`[${KEY_TABLE_ATTR}]`)?.getAttribute(KEY_TABLE_ATTR);
  if (named) return [named];
  for (const [sel, names] of OVERLAY_OF) if (origin.closest(sel)) return [...names];
  // Any other dialog: the overlay whose name its title spells.
  const dlg = origin.closest('dialog');
  const label = (dlg?.getAttribute('aria-label') ?? dlg?.querySelector('h2')?.textContent ?? '').trim().toLowerCase();
  return label ? [label] : [];
}

export function openKeyHelp(snap: UiSnapshot): void {
  const origin = document.activeElement as HTMLElement | null;
  const here = overlayNames(origin);
  let dlg = document.getElementById('keys-help') as HTMLDialogElement | null;
  if (!dlg) {
    dlg = document.createElement('dialog');
    dlg.id = 'keys-help';
    dlg.className = 'keys-help';
    dlg.setAttribute('aria-label', 'Keyboard help');
    document.body.append(dlg);
  }
  const d = dlg;
  const vimOn = snap.vimKeys;
  const views: Record<string, HelpRow[]> = {
    contextual: [
      ...plainRows(overlayRows(matchOverlay(here)), vimOn),
      ...plainRows(contextualRows({ ...snap, modalOpen: false }, currentPlatform()), vimOn, true),
    ],
    all: plainRows([...allRows(), ...allOverlayRows()], vimOn),
    symbols: plainRows(symbolRows(), vimOn, true),
    start: gettingStartedRows(currentPlatform()),
  };
  // Getting started opens first with Vim keys off or the first time Help is opened (local #227).
  let seen = false;
  try { seen = localStorage.getItem(SEEN_KEY) === '1'; localStorage.setItem(SEEN_KEY, '1'); } catch { /* no storage: always show it */ }
  let st: HelpState = initialHelp(vimOn, seen);
  const tabs = el('div', 'keys-help-tabs');
  // A real search box: typing filters the rows with Vim keys off or on (`/` focuses it with Vim keys on).
  const search = el('input', 'keys-help-search') as HTMLInputElement;
  search.type = 'search';
  search.placeholder = 'Search shortcuts and actions';
  search.setAttribute('aria-label', 'Search the shortcuts');
  search.autocomplete = 'off';
  search.spellcheck = false;
  const body = el('div', 'keys-help-body');
  body.tabIndex = -1;
  const foot = el('div', 'keys-help-foot', helpFooter(vimOn, currentPlatform()));
  d.replaceChildren(tabs, search, body, foot);

  const shown = () => filterRows(views[st.view] ?? [], st.query);
  const viewButtons: { v: string; t: HTMLButtonElement }[] = [];
  const draw = () => {
    // The view buttons are built once and toggled in place: a rebuild would drop the focused one (local #243).
    if (viewButtons.length === 0) {
      for (const v of HELP_VIEWS) {
        const t = el('button', 'seg', VIEW_TITLES[v]);
        t.type = 'button';
        t.addEventListener('click', () => { st = { ...st, view: v, scroll: 0 }; draw(); });
        viewButtons.push({ v, t });
      }
      tabs.replaceChildren(...viewButtons.map((b) => b.t));
    }
    for (const b of viewButtons) b.t.className = `seg${b.v === st.view ? ' active' : ''}`;
    if (search.value !== st.query) search.value = st.query;
    const rows = shown();
    const frag = document.createDocumentFragment();
    let section = '';
    rows.forEach((r, i) => {
      if (r.section !== section) {
        section = r.section;
        frag.append(el('div', 'keys-help-section', section));
      }
      const row = el('div', `keys-help-row${r.bound ? '' : ' unbound'}`);
      row.dataset.index = String(i);
      const keys = el('span', 'k');
      for (const k of r.keys) keys.append(el('kbd', '', hintLabel(k, currentPlatform())));
      // The action in words first; its keys are the secondary column.
      row.append(el('span', 't', r.title), el('span', 'd', r.detail), keys);
      frag.append(row);
    });
    if (rows.length === 0) frag.append(el('div', 'pick-empty', st.query ? 'Nothing matches every term' : 'Nothing to list'));
    body.replaceChildren(frag);
    const target = body.querySelector<HTMLElement>(`[data-index="${st.scroll}"]`);
    if (target) body.scrollTop = Math.max(0, target.offsetTop - body.offsetTop - ROW_PX);
    else body.scrollTop = 0;
  };
  search.addEventListener('input', () => { st = { ...st, query: search.value, scroll: 0 }; draw(); });
  d.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    // Ctrl-Alt-G closes it too (it opened it).
    if (ev.ctrlKey && ev.altKey && ev.key.toLowerCase() === 'g') { ev.preventDefault(); d.close(); return; }
    if (ev.metaKey || (ev.altKey && !ev.ctrlKey)) return;
    if (ev.target === search) {
      // In the search box letters are typing. Esc clears it (then closes), Enter or Down goes on to the rows.
      if (ev.key === 'Escape') {
        ev.preventDefault();
        ev.stopPropagation();
        if (search.value) { st = { ...st, query: '', scroll: 0 }; draw(); } else d.close();
      } else if (ev.key === 'Enter' || ev.key === 'ArrowDown') {
        ev.preventDefault();
        body.focus();
      }
      return;
    }
    // Vim keys off: Tab and Shift-Tab are the browser's focus order; F6 switches the view instead.
    if (tabIsNative(vimOn, ev)) return;
    const fx = helpKey(st, { key: paneKeyName(ev), ctrl: ev.ctrlKey, shift: ev.shiftKey }, shown().length);
    if (fx.kind === 'none') return;
    ev.preventDefault();
    ev.stopPropagation();
    if (fx.kind === 'close') { d.close(); return; }
    // `/` moves to the search box instead of a hidden typing mode.
    if (fx.st.searching) { search.focus(); search.select(); return; }
    st = fx.st;
    draw();
  };
  d.onclose = () => { if (origin && document.contains(origin)) origin.focus({ preventScroll: true }); };
  d.onclick = (ev) => { if (ev.target === d) d.close(); };
  draw();
  if (!d.open) d.showModal();
  // With Vim keys off typing is the way in: the search box has the focus; with them on the rows do (j/k scroll).
  if (vimOn) body.focus(); else search.focus();
}
