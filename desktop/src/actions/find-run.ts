// Find in the open conversation, DOM side (local #307; spec in actions/find.ts, matching in find-model.ts).
// A find bar sits between the session header and the transcript, so it never covers the message box. The match list
// is built from every loaded event (state.events), not from the rendered window: a match behind "Show earlier" grows
// the window to include it. Highlights wrap text nodes in <mark> (never innerHTML), so rendered Markdown keeps its
// structure, and are redone after every transcript rebuild or late Markdown fill by one MutationObserver.
// The session-list filter (#search) is never read or written here.
import { type SliceRuntime, dispatch } from '../commands.js';
import { onJumpLatest, renderEvents, settleScroll } from '../conversation.js';
import { $, el, toast } from '../dom.js';
import {
  findLabel, findMatches, isFindChord, keepCurrent, limitToReveal, normalizeNeedle, splitMatches, stepFind,
} from '../find-model.js';
import { focusList } from '../focus-return.js';
import { focusOwnsKey } from '../modal-gate.js';
import { state } from '../state.js';
import { currentPlatform, isVimKeysOn } from '../vimkeys.js';
import { detailSnapshot, setDetailFocus, vis } from './detail.js';
import { SLICE } from './find.js';

const f = {
  open: false,
  /** The query being searched; kept after Esc so n / N can carry on (the TUI keeps its last search). */
  needle: '',
  /** Event indices (into state.events) that match, in transcript order. */
  matches: [] as number[],
  /** The current match: an index into state.events. */
  current: null as number | null,
  epoch: -1,
  opener: null as HTMLElement | null,
  wasTranscript: false,
  /** <details> that find opened to show the current match; closed again when it moves on. */
  opened: new Set<HTMLDetailsElement>(),
  timer: 0,
  /** Until this time (ms) a rebuild or a late Markdown fill re-scrolls to the current match; any scroll by the operator ends it. */
  pinUntil: 0,
};
/** Markdown fills in after a rebuild and changes event heights; the match is kept in view for this long. */
const PIN_MS = 3000;
let bar: HTMLElement | null = null;
let input: HTMLInputElement | null = null;
let count: HTMLElement | null = null;
let observer: MutationObserver | null = null;

/** A conversation is on screen (an Issues or Settings view hides the session view but leaves the selection). */
function conversationShown(): boolean {
  return state.selectedId !== null && !$('session-view').hidden;
}

/** Whether a find query is open or remembered, so n / N can step it. */
export function findActive(): boolean {
  return conversationShown() && (f.open || normalizeNeedle(f.needle) !== '');
}

// ---------- the one place that scrolls to a match ----------

/**
 * Bring a match into view. Every scroll the find bar makes goes through here, so it has one place to reconcile with the
 * conversation's scroll anchoring (#306): the jump is the reader's new place (the next rebuild anchors to the event now at
 * the top of the viewport, so it cannot pull the pane back), and it is not an arrival (the "N new" count is left alone,
 * and "Jump to latest" stays valid; only landing on the tail clears it).
 */
function scrollToMatch(target: HTMLElement): void {
  target.scrollIntoView({ block: 'center' });
  settleScroll();
}

// ---------- highlights ----------

function clearMarks(): void {
  const pane = $('events');
  for (const m of pane.querySelectorAll<HTMLElement>('mark.find-hit')) {
    const parent = m.parentNode;
    if (!parent) continue;
    parent.replaceChild(document.createTextNode(m.textContent ?? ''), m);
    parent.normalize();
  }
  for (const n of pane.querySelectorAll('.ev.search-hit')) n.classList.remove('search-hit');
}

/** Wrap each occurrence of the needle inside `node`'s text (not its time stamp or buttons) in a <mark>. */
function markNode(node: HTMLElement, needle: string): void {
  const walker = document.createTreeWalker(node, NodeFilter.SHOW_TEXT, {
    acceptNode: (t) => (t.parentElement?.closest('.ev-time, button, mark.find-hit') ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
  });
  const texts: Text[] = [];
  for (let t = walker.nextNode(); t; t = walker.nextNode()) texts.push(t as Text);
  for (const t of texts) {
    const parts = splitMatches(t.data, needle);
    if (!parts.some((p) => p.hit)) continue;
    const frag = document.createDocumentFragment();
    for (const p of parts) {
      if (p.hit) frag.append(el('mark', 'find-hit', p.text));
      else frag.append(document.createTextNode(p.text));
    }
    t.replaceWith(frag);
  }
}

function currentNode(): HTMLElement | null {
  const e = f.current === null ? undefined : state.events[f.current];
  return e ? $('events').querySelector<HTMLElement>(`.ev[data-seq="${e.sequence}"]`) : null;
}

/** Redo every highlight for the rendered events; returns the current match's node (its first mark current). */
function highlight(): HTMLElement | null {
  observer?.disconnect();
  try {
    clearMarks();
    const needle = f.needle;
    const seqs = new Set(f.matches.map((i) => state.events[i]!.sequence));
    for (const node of $('events').querySelectorAll<HTMLElement>('.ev')) {
      if (!seqs.has(Number(node.dataset.seq))) continue;
      markNode(node, needle);
    }
    const node = currentNode();
    // Close what find opened for an earlier match; open what the current one sits in.
    for (const d of f.opened) if (!node?.contains(d)) { d.open = false; f.opened.delete(d); }
    if (!node) return null;
    node.classList.add('search-hit');
    const first = node.querySelector<HTMLElement>('mark.find-hit');
    first?.classList.add('current');
    for (let d = first?.closest('details') ?? null; d; d = d.parentElement?.closest('details') ?? null) {
      if (!d.open) { d.open = true; f.opened.add(d); }
    }
    return first ?? node;
  } finally {
    observe();
  }
}

function observe(): void {
  if (f.open) observer?.observe($('events'), { childList: true, subtree: true });
}

// ---------- matches ----------

function renderCount(): void {
  if (count) count.textContent = findLabel(f.matches, f.current, f.needle);
  input?.setAttribute('aria-invalid', f.matches.length === 0 && normalizeNeedle(f.needle) !== '' ? 'true' : 'false');
}

function recompute(keep: boolean): void {
  f.matches = findMatches(state.events, f.needle, vis());
  f.current = keepCurrent(f.matches, keep ? f.current : null);
}

/** Show the current match: grow the window to include it, highlight, and scroll to it. */
function showCurrent(): void {
  if (f.current !== null) {
    const limit = limitToReveal(state.events.length, f.current, state.renderLimit);
    if (limit !== state.renderLimit) {
      state.renderLimit = limit;
      state.renderedCount = 0;
      renderEvents();
    }
  }
  const target = highlight();
  renderCount();
  f.pinUntil = Date.now() + PIN_MS;
  if (target) scrollToMatch(target);
}

function go(dir: 1 | -1): void {
  if (!conversationShown()) return;
  f.needle = input?.value ?? f.needle;
  const from = f.current;
  f.matches = findMatches(state.events, f.needle, vis());
  f.current = stepFind(f.matches, from, dir);
  showCurrent();
}

/** The transcript changed under an open bar (live events, Markdown filled in, the window grew). */
function refresh(): void {
  f.timer = 0;
  if (!f.open) return;
  if (state.selectionEpoch !== f.epoch || !conversationShown()) { closeFind(false); f.needle = ''; return; }
  recompute(true);
  const target = highlight();
  renderCount();
  if (target && Date.now() < f.pinUntil) scrollToMatch(target);
}

function scheduleRefresh(): void {
  if (f.timer === 0) f.timer = window.setTimeout(refresh, 60);
}

// ---------- the bar ----------

function build(): void {
  if (bar) return;
  bar = el('div', 'find-bar');
  bar.id = 'find-bar';
  bar.hidden = true;
  bar.setAttribute('role', 'search');
  bar.setAttribute('aria-label', 'Find in this conversation');
  input = el('input');
  input.type = 'text';
  input.id = 'find-input';
  input.placeholder = 'Find in this conversation…';
  input.setAttribute('aria-label', 'Find in this conversation');
  input.autocomplete = 'off';
  input.spellcheck = false;
  count = el('span', 'find-count');
  count.id = 'find-count';
  count.setAttribute('role', 'status');
  count.setAttribute('aria-live', 'polite');
  const button = (text: string, label: string, run: () => void): HTMLButtonElement => {
    const b = el('button', 'btn', text);
    b.type = 'button';
    b.title = label;
    b.setAttribute('aria-label', label);
    b.addEventListener('click', run);
    return b;
  };
  const prev = button('↑', 'Previous match (Shift+Enter)', () => go(-1));
  const next = button('↓', 'Next match (Enter)', () => go(1));
  const close = button('Close', 'Close find (Esc)', () => closeFind(true));
  bar.append(input, count, prev, next, close);
  $('events').before(bar);
  input.addEventListener('input', () => {
    f.needle = input!.value;
    recompute(true);
    showCurrent();
  });
  input.addEventListener('keydown', (ev) => {
    if (focusOwnsKey(ev.key, ev.target as Element | null)) return;
    if (ev.key === 'Enter') { ev.preventDefault(); ev.stopPropagation(); go(ev.shiftKey ? -1 : 1); }
    else if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); closeFind(true); }
  });
  observer = new MutationObserver(scheduleRefresh);
  // The operator scrolling ends the pin: their position wins from then on.
  for (const t of ['wheel', 'touchmove', 'pointerdown'] as const) $('events').addEventListener(t, () => { f.pinUntil = 0; }, { passive: true });
  onJumpLatest(() => { f.pinUntil = 0; });
}

/** Open the bar on the open conversation (focus the box, search what is already typed). */
export function openFind(focusBox = true, show = true): void {
  if (!conversationShown()) { toast('Open a session first: click one in the list (j/k, then Enter)'); return; }
  build();
  if (!f.open) {
    const a = document.activeElement as HTMLElement | null;
    f.opener = a && a !== document.body && !bar!.contains(a) ? a : null;
    f.wasTranscript = detailSnapshot().focus === 'transcript';
    f.open = true;
    f.epoch = state.selectionEpoch;
    bar!.hidden = false;
  }
  input!.value = f.needle;
  if (focusBox) { input!.focus(); input!.select(); }
  if (!show) return;
  recompute(true);
  showCurrent();
}

/** Close the bar: the highlights go and focus returns to where it was (the transcript, a control or the list). */
export function closeFind(restoreFocus: boolean): void {
  if (!f.open) return;
  f.open = false;
  observer?.disconnect();
  if (f.timer !== 0) { window.clearTimeout(f.timer); f.timer = 0; }
  if (bar) bar.hidden = true;
  clearMarks();
  for (const d of f.opened) d.open = false;
  f.opened.clear();
  f.matches = [];
  f.current = null;
  if (!restoreFocus) return;
  if (f.wasTranscript) setDetailFocus('transcript');
  const back = f.opener;
  f.opener = null;
  if (back?.isConnected) back.focus();
  else if (!f.wasTranscript) focusList();
  else (document.activeElement as HTMLElement | null)?.blur?.();
}

/** `/` with the transcript keyed searches the conversation; false leaves `/` to the session filter. */
export function findSlash(): boolean {
  if (!conversationShown() || detailSnapshot().focus !== 'transcript') return false;
  openFind();
  return true;
}

/** `n` / `N`: step the conversation's matches. False when there is nothing to step (the caller falls back to the list). */
export function findStepKey(dir: 1 | -1): boolean {
  if (!findActive()) return false;
  if (!f.open) openFind(false, false);
  go(dir);
  return true;
}

// ---------- wiring ----------

function init(): void {
  // Cmd-F on macOS, Ctrl-F elsewhere with Vim keys off (with them on Ctrl-F is the transcript's page-down).
  // Capture on the window, first: ahead of the navigation slice's Ctrl-F and the vim engine's Cmd-as-Ctrl.
  window.addEventListener('keydown', (ev) => {
    const platform = currentPlatform();
    if (!isFindChord(ev, platform)) return;
    if (platform !== 'mac' && isVimKeysOn()) return;
    if (document.querySelector('dialog[open]') !== null || !conversationShown()) return;
    ev.preventDefault();
    ev.stopImmediatePropagation();
    if (f.open) { input?.focus(); input?.select(); return; }
    dispatch('find-in-conversation', { source: 'key' });
  }, true);
}

export const RUNTIME: SliceRuntime = {
  slice: SLICE,
  run: (id) => { if (id === 'find-in-conversation') openFind(); },
  init,
  // Ahead of every other slice's window listener (the navigation slice routes Ctrl-F at 900).
  initOrder: 3,
};
