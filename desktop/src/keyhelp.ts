// The keybindings help overlay (DOM side of keyhelp-model.ts; slice 7). It is
// built from the snapshot captured when it opens, so the Contextual view lists
// what could run where the operator was. Closing it returns focus there.
import { el } from './dom.js';
import {
  HELP_VIEWS, type HelpRow, type HelpState, INITIAL_HELP, VIEW_TITLES, allOverlayRows, allRows, contextualRows, filterRows, helpKey,
  overlayRows, symbolRows,
} from './keyhelp-model.js';
import type { UiSnapshot } from './registry.js';
import { currentPlatform } from './vimkeys.js';
import { focusOwnsKey } from './modal-gate.js';

const ROW_PX = 22;

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

function overlayNames(origin: Element | null): string[] {
  if (!origin) return [];
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
  const views: Record<string, HelpRow[]> = {
    contextual: [...overlayRows(matchOverlay(here)), ...contextualRows({ ...snap, modalOpen: false }, currentPlatform())],
    all: [...allRows(), ...allOverlayRows()],
    symbols: symbolRows(),
  };
  let st: HelpState = INITIAL_HELP;
  const tabs = el('div', 'keys-help-tabs');
  const search = el('div', 'keys-help-search');
  const body = el('div', 'keys-help-body');
  body.tabIndex = -1;
  const foot = el('div', 'keys-help-foot', 'Tab views · s symbols · / search · j/k scroll · Ctrl-d/u half page · g/G ends · Esc clears or closes · q closes · Ctrl-K opens the command palette');
  d.replaceChildren(tabs, search, body, foot);

  const shown = () => filterRows(views[st.view] ?? [], st.query);
  const draw = () => {
    tabs.replaceChildren(...HELP_VIEWS.map((v) => {
      const t = el('button', `seg${v === st.view ? ' active' : ''}`, VIEW_TITLES[v]);
      t.type = 'button';
      t.addEventListener('click', () => { st = { ...st, view: v, scroll: 0 }; draw(); });
      return t;
    }));
    search.textContent = st.searching || st.query ? `/${st.query}${st.searching ? '▏' : ''}` : '';
    search.hidden = !(st.searching || st.query);
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
      for (const k of r.keys) keys.append(el('kbd', '', k));
      row.append(keys, el('span', 't', r.title), el('span', 'd', r.detail));
      frag.append(row);
    });
    if (rows.length === 0) frag.append(el('div', 'pick-empty', st.query ? 'Nothing matches every term' : 'Nothing to list'));
    body.replaceChildren(frag);
    const target = body.querySelector<HTMLElement>(`[data-index="${st.scroll}"]`);
    if (target) body.scrollTop = Math.max(0, target.offsetTop - body.offsetTop - ROW_PX);
    else body.scrollTop = 0;
  };
  d.onkeydown = (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.isComposing) return;
    // Ctrl-Alt-G closes it too (it opened it).
    if (ev.ctrlKey && ev.altKey && ev.key.toLowerCase() === 'g') { ev.preventDefault(); d.close(); return; }
    if (ev.metaKey || (ev.altKey && !ev.ctrlKey)) return;
    const fx = helpKey(st, { key: ev.key, ctrl: ev.ctrlKey, shift: ev.shiftKey }, shown().length);
    if (fx.kind === 'none') return;
    ev.preventDefault();
    ev.stopPropagation();
    if (fx.kind === 'close') { d.close(); return; }
    st = fx.st;
    draw();
  };
  d.onclose = () => { if (origin && document.contains(origin)) origin.focus({ preventScroll: true }); };
  d.onclick = (ev) => { if (ev.target === d) d.close(); };
  draw();
  if (!d.open) d.showModal();
  body.focus();
}
