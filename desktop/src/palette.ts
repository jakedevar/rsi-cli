// Command palette DOM: a modal <dialog> listing every registry action with its
// keys and disabled state; a query starting with `:` also offers to run it as
// a TUI command line (the TUI's own palette is the `:` line). Ranking and the
// highlight move are pure (registry.ts); this file only renders and routes.
import { dispatch, snapshot } from './commands.js';
import type { UiSnapshot } from './registry.js';
import { $, el, toast } from './dom.js';
import {
  type PaletteEntry, type Ranked, collapsedCount, commandQuery, isPaletteChord, movePick, paletteEntries, rankPalette,
} from './registry.js';
import { currentPlatform, runCommandLine } from './vimkeys.js';
import { paletteArgument } from './overlay-keys.js';
import { focusOwnsKey } from './modal-gate.js';
import { syncListbox } from './listbox-a11y.js';

let entries: PaletteEntry[] = [];
let ranked: Ranked[] = [];
/** The `:` command the query spells, shown as row 0. */
let cmd: string | null = null;
let pick = 0;
/** The Vim keys switch when the palette opened (its empty-result line and rows follow it). */
let vimOn = true;
/** Tab put the highlighted command on the line to edit its argument: the query to go back to (gap sweep G4). */
let argFrom: string | null = null;

const total = () => ranked.length + (cmd === null ? 0 : 1);

const dialog = () => $<HTMLDialogElement>('palette');
const input = () => $<HTMLInputElement>('palette-input');

export function isPaletteOpen(): boolean {
  return dialog().open;
}

/** `snap` is for tests; the palette reads the live snapshot. */
export function openPalette(snap: UiSnapshot = snapshot()): void {
  const d = dialog();
  vimOn = snap.vimKeys;
  entries = paletteEntries(snap, currentPlatform());
  input().value = '';
  argFrom = null;
  rerank();
  if (!d.open) d.showModal();
  input().focus();
}

export function closePalette(): void {
  if (dialog().open) dialog().close();
}

function rerank(): void {
  ranked = rankPalette(entries, input().value);
  cmd = commandQuery(input().value);
  pick = total() > 0 ? 0 : -1;
  render();
}

/** Title text with the matched characters wrapped in <mark> (textContent only). */
function titleNode(r: Ranked): HTMLElement {
  const span = el('span', 'palette-title');
  const hits = new Set(r.hits);
  let run = '';
  let marked = false;
  const flush = () => {
    if (!run) return;
    span.append(marked ? el('mark', undefined, run) : run);
    run = '';
  };
  [...r.entry.title].forEach((ch, i) => {
    const m = hits.has(i);
    if (m !== marked) { flush(); marked = m; }
    run += ch;
  });
  flush();
  return span;
}

/** An empty query hides the actions that wait on a screen that is not open; this line says so (not an option row). */
function hiddenNote(): HTMLElement[] {
  const n = input().value.trim() === '' ? collapsedCount(entries) : 0;
  if (n === 0) return [];
  const li = el('li', 'palette-empty', `${n} more actions work once their screen is open (such as the Issues workspace). Type to find them.`);
  li.setAttribute('role', 'note');
  return [li];
}

function render(): void {
  const list = $('palette-list');
  if (total() === 0) {
    list.replaceChildren(el('li', 'palette-empty', vimOn
      ? 'No matching action (start with : to run a TUI command)'
      : 'No matching action. Try one plain word, such as “files”, “help” or “stop”.'));
    syncListbox(list, input());
    return;
  }
  const off = cmd === null ? 0 : 1;
  const rows: HTMLElement[] = [];
  if (cmd !== null) {
    const li = el('li', `palette-item${pick === 0 ? ' sel' : ''}`);
    li.dataset.index = '0';
    li.setAttribute('role', 'option');
    li.setAttribute('aria-selected', String(pick === 0));
    const main = el('div', 'palette-main');
    main.append(el('span', 'palette-title', `Run :${cmd}`), el('span', 'palette-detail', 'TUI command line'));
    const keys = el('span', 'palette-keys');
    keys.append(el('kbd', undefined, 'Enter'));
    li.append(el('span', 'palette-group', 'command'), main, keys);
    rows.push(li);
  }
  list.replaceChildren(...rows, ...ranked.map((r, j) => {
    const i = j + off;
    const e = r.entry;
    const li = el('li', `palette-item${i === pick ? ' sel' : ''}${e.enabled ? '' : ' disabled'}`);
    li.dataset.index = String(i);
    li.setAttribute('role', 'option');
    li.setAttribute('aria-selected', String(i === pick));
    if (!e.enabled) li.setAttribute('aria-disabled', 'true');
    const main = el('div', 'palette-main');
    main.append(titleNode(r));
    const sub = e.enabled ? e.detail : e.why;
    if (sub) main.append(el('span', 'palette-detail', sub));
    const keys = el('span', 'palette-keys');
    for (const k of e.keys) keys.append(el('kbd', undefined, k));
    li.append(el('span', 'palette-group', e.group), main, keys);
    li.title = e.enabled ? e.title : `${e.title}: ${e.why ?? 'unavailable'}`;
    return li;
  }), ...hiddenNote());
  list.querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
  syncListbox(list, input());
}

/** Move the highlight without rebuilding the rows (a click may be in flight). */
function highlight(i: number, scroll = true): void {
  pick = i;
  for (const li of $('palette-list').querySelectorAll<HTMLElement>('.palette-item')) {
    const on = Number(li.dataset.index) === i;
    li.classList.toggle('sel', on);
    li.setAttribute('aria-selected', String(on));
    if (on && scroll) li.scrollIntoView({ block: 'nearest' });
  }
  syncListbox($('palette-list'), input());
}

function runPick(i: number): void {
  if (cmd !== null && i === 0) {
    const line = cmd;
    closePalette();
    runCommandLine(line);
    return;
  }
  const r = ranked[i - (cmd === null ? 0 : 1)];
  if (!r) return;
  closePalette();
  if (!r.entry.enabled) { toast(r.entry.why ?? `${r.entry.title} is not available now`); return; }
  dispatch(r.entry.id, { source: 'palette' });
}

export function initPalette(): void {
  input().addEventListener('input', rerank);
  input().addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    const ctrl = ev.ctrlKey && !ev.metaKey && !ev.altKey;
    let delta = 0;
    if (ev.key === 'ArrowDown' || (ctrl && ev.key === 'n')) delta = 1;
    else if (ev.key === 'ArrowUp' || (ctrl && ev.key === 'p')) delta = -1;
    else if (ev.key === 'PageDown') delta = 8;
    else if (ev.key === 'PageUp') delta = -8;
    if (delta !== 0) {
      ev.preventDefault();
      highlight(movePick(total(), pick, delta));
    } else if (ev.key === 'Enter' && !ev.isComposing) {
      ev.preventDefault();
      runPick(pick);
    } else if (ev.key === 'Tab' && !ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey) {
      // Tab edits the highlighted command's argument on the `:` line (the TUI palette's argument edit).
      ev.preventDefault();
      const r = ranked[pick - (cmd === null ? 0 : 1)];
      const line = r ? paletteArgument(r.entry.searchKeys) : null;
      if (!line) { if (r) toast(`${r.entry.title} has no : command to give an argument`, true); return; }
      argFrom ??= input().value;
      input().value = line;
      rerank();
    } else if (ev.key === 'Escape' && argFrom !== null) {
      // Esc leaves the argument edit first, then closes.
      ev.preventDefault();
      ev.stopPropagation();
      input().value = argFrom;
      argFrom = null;
      rerank();
    }
  });
  const list = $('palette-list');
  list.addEventListener('click', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.palette-item');
    if (li) runPick(Number(li.dataset.index));
  });
  list.addEventListener('mousemove', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.palette-item');
    const i = li ? Number(li.dataset.index) : -1;
    if (i >= 0 && i !== pick) highlight(i, false);
  });
  // A click on the backdrop (outside the panel) closes it.
  dialog().addEventListener('click', (ev) => { if (ev.target === dialog()) closePalette(); });
  // Ctrl-K / Cmd-K anywhere, ahead of the vim keys and with them off.
  window.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    const t = document.activeElement as HTMLElement | null;
    const typing = !!t && (t.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(t.tagName));
    if (ev.ctrlKey && t?.closest('.terminal-overlay')) return; // the focused shell owns Ctrl-K (slice 12)
    if (!isPaletteChord(ev, currentPlatform(), typing && t !== input())) return;
    ev.preventDefault();
    ev.stopPropagation();
    dispatch('palette', { source: 'key' });
  }, { capture: true });
}
