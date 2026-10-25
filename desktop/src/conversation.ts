// Tool/thinking/system text is rendered with textContent. User and assistant
// messages go through markdown.ts (Rust-sanitized HTML, the only innerHTML path).
// Conversation pane: event rendering and incremental polling.
import { quietRebuild } from './a11y.js';
import { api, type ConversationEvent } from './api.js';
import { $, el, toast } from './dom.js';
import { markdownBody } from './markdown.js';
import { splitFold } from './mdmodel.js';
import { FOLD_CHARS, elideBlobs, eventKind, maxSequence, mergeEvents } from './model.js';
import { type Anchor, atBottom, jumpLabel, planWindow, restoredScrollTop } from './scroll-model.js';
import { RENDER_PAGE, state } from './state.js';
import { isErrorEvent } from './session-outcome-model.js';
import { renderOutcome } from './session-outcome.js';
import { type Projection, callNote, project, resultNote } from './tool-projection.js';

/** Which result answers which call (tool-projection.ts), for the summaries. */
let projection: Projection = project([]);

export function eventNode(e: ConversationEvent): HTMLElement {
  const kind = eventKind(e);
  const box = el('div', `ev ev-${kind}${isErrorEvent(e) ? ' ev-error' : ''}`);
  box.dataset.seq = String(e.sequence);
  const when = new Date(e.created_at).toLocaleTimeString();
  if (kind === 'tool' || kind === 'result' || kind === 'thinking') {
    const d = el('details');
    const label = kind === 'tool' ? `▸ ${e.tool_name ?? 'tool'}`
      : kind === 'result' ? `◂ result${e.tool_name ? ` (${e.tool_name})` : ''}`
      : '… thinking';
    const firstLine = elideBlobs(e.content).split('\n', 1)[0]?.slice(0, 100) ?? '';
    const summary = el('summary', undefined, `${when}  ${label}  ${firstLine}`);
    const note = kind === 'tool' ? callNote(projection, e.sequence) : kind === 'result' ? resultNote(projection, e.sequence) : '';
    const pair = el('span', 'ev-pair', note ? `  ${note}` : '');
    summary.append(pair);
    d.append(summary);
    let body = e.content;
    if (kind === 'tool' && e.tool_input != null) {
      try { body = JSON.stringify(e.tool_input, null, 2); } catch { /* keep content */ }
    }
    d.append(el('pre', undefined, elideBlobs(body)));
    box.append(d);
  } else {
    // Elide blobs first, then fold, then render each part as Markdown.
    const text = elideBlobs(e.content);
    box.append(el('span', 'ev-time', when));
    const [head, tail] = splitFold(text, FOLD_CHARS);
    box.append(markdownBody(e.session_id, e, 0, head));
    if (tail) {
      const d = el('details');
      d.append(el('summary', undefined, `${tail.length} more characters`));
      d.append(markdownBody(e.session_id, e, 1, tail));
      box.append(d);
    }
  }
  return box;
}

/** Hide or show tool calls and results in the conversation pane. */
export function setHideTools(hide: boolean): void {
  state.hideTools = hide;
  $('events').classList.toggle('hide-tools', hide);
  $('toggle-tools').textContent = hide ? 'Show tool calls' : 'Hide tool calls';
}

let wired = false;

function jumpButton(): HTMLButtonElement | null {
  return document.getElementById('jump-latest') as HTMLButtonElement | null;
}

/** Show or hide the "Jump to latest (N new)" control from `state.unseenEvents`. */
function syncJump(): void {
  const b = jumpButton();
  if (!b) return;
  const n = state.unseenEvents;
  b.hidden = n <= 0;
  if (n > 0) b.textContent = jumpLabel(n);
}

/** Scroll the conversation to its newest event and forget the unseen count (instant: no animation to reduce). */
export function scrollToLatest(): void {
  const pane = $('events');
  pane.scrollTop = pane.scrollHeight;
  state.unseenEvents = 0;
  syncJump();
  for (const fn of jumpListeners) fn();
}

/**
 * Settle the pane after code (the find bar, #307) moved its scroll position on purpose. The jump is not an event arriving,
 * so the unseen count is never touched on the way there; landing at the bottom reads the tail (count cleared), landing
 * anywhere else leaves the button as it was so "Jump to latest" still counts what arrives next and still works.
 * The next rebuild anchors to whatever event the reader is now looking at, so it cannot undo the jump.
 */
export function settleScroll(): void {
  const pane = $('events');
  if (state.unseenEvents > 0 && atBottom(pane)) state.unseenEvents = 0;
  syncJump();
}

const jumpListeners: Array<() => void> = [];
/** Called whenever the pane jumps to the newest event ("Jump to latest", G, End): code that pins the view elsewhere (find) lets go. */
export function onJumpLatest(fn: () => void): void { jumpListeners.push(fn); }

/** A new session was opened: nothing is unseen. */
export function resetScroll(): void {
  state.unseenEvents = 0;
  syncJump();
}

function wireScroll(pane: HTMLElement): void {
  if (wired) return;
  wired = true;
  // Reaching the bottom by any means (wheel, keys, the button) clears the unseen count.
  pane.addEventListener('scroll', () => {
    if (state.unseenEvents > 0 && atBottom(pane)) { state.unseenEvents = 0; syncJump(); }
  }, { passive: true });
  jumpButton()?.addEventListener('click', () => {
    scrollToLatest();
    // The button hides itself: keep the keys with the conversation, not on the page body.
    pane.tabIndex = -1;
    pane.focus({ preventScroll: true });
  });
}

function captureAnchor(pane: HTMLElement): Anchor | null {
  const top = pane.getBoundingClientRect().top;
  for (const n of Array.from(pane.children) as HTMLElement[]) {
    if (!n.classList.contains('ev')) continue;
    const r = n.getBoundingClientRect();
    if (r.height > 0 && r.bottom > top) return { seq: Number(n.dataset.seq), offset: r.top - top };
  }
  return null;
}

function restoreAnchor(pane: HTMLElement, a: Anchor): boolean {
  const node = pane.querySelector<HTMLElement>(`.ev[data-seq="${a.seq}"]`);
  if (!node) return false;
  pane.scrollTop = restoredScrollTop(pane.scrollTop, node.getBoundingClientRect().top, pane.getBoundingClientRect().top, a.offset);
  return true;
}

/** The "Show earlier" button at the top of the pane: created, relabelled or removed to match `hidden`. */
function syncMore(pane: HTMLElement, hidden: number): void {
  const first = pane.children[0] as HTMLElement | undefined;
  const has = first?.classList.contains('more') === true;
  if (hidden <= 0) { if (has) first!.remove(); return; }
  const label = `Show earlier (${hidden} hidden)`;
  if (has) { first!.textContent = label; return; }
  const more = el('button', 'btn more', label);
  more.type = 'button';
  more.addEventListener('click', () => renderEvents({ earlier: true }));
  pane.prepend(more);
}

/**
 * Draw the open session's events. Opening a session paints the last `renderLimit` events and follows the tail.
 * After that the DOM is patched, never rebuilt: new events are appended, "Show earlier" prepends a page, and
 * a window that outgrew its limit is trimmed from the top. A reader within NEAR_BOTTOM_PX of the end follows
 * the tail; anyone further up keeps the event at the top of the viewport where it was (anchor by element and
 * offset, so prepend and trim never move it), and a "Jump to latest (N new)" button counts what arrived.
 */
export function renderEvents(opts: { earlier?: boolean } = {}): void {
  const pane = $('events');
  wireScroll(pane);
  const evs = state.events;
  const stick = atBottom(pane);
  const opening = state.renderedFirstSeq === undefined;
  const anchor = stick ? null : captureAnchor(pane);
  const domIdx = state.renderedCount > 0 && state.renderedFirstSeq !== undefined
    ? evs.findIndex((e) => e.sequence === state.renderedFirstSeq) : -1;
  projection = project(evs);
  let appended = 0;
  let start: number;
  if (domIdx >= 0 && evs.length - domIdx >= state.renderedCount) {
    if (opts.earlier) state.renderLimit += RENDER_PAGE;
    const anchorIdx = anchor ? evs.findIndex((e) => e.sequence === anchor.seq) : null;
    const plan = planWindow(evs.length, domIdx, state.renderLimit, { earlier: opts.earlier === true, anchorIdx, atBottom: stick });
    start = plan.start;
    const fresh = evs.slice(domIdx + state.renderedCount);
    for (const e of fresh) {
      pane.append(eventNode(e));
      if (!(state.hideTools && (eventKind(e) === 'tool' || eventKind(e) === 'result'))) appended += 1;
    }
    // A result that just arrived updates its call's summary (and its batch leader's).
    for (const e of fresh) {
      const call = projection.results.get(e.sequence)?.call;
      if (call == null) continue;
      for (const seq of new Set([call, projection.calls.get(call)?.leader ?? call])) {
        const span = pane.querySelector<HTMLElement>(`.ev[data-seq="${seq}"] .ev-pair`);
        if (span) { const n = callNote(projection, seq); span.textContent = n ? `  ${n}` : ''; }
      }
    }
    if (plan.shift > 0) {
      // Trim the top: drop the nodes before the new first event.
      const keep = evs[start]!.sequence;
      for (const n of Array.from(pane.children) as HTMLElement[]) {
        if (!n.classList.contains('ev')) continue;
        if (Number(n.dataset.seq) >= keep) break;
        n.remove();
      }
    } else if (plan.shift < 0) {
      const first = pane.querySelector<HTMLElement>('.ev');
      const older = evs.slice(start, domIdx).map(eventNode);
      if (first) first.before(...older); else pane.append(...older);
    }
    syncMore(pane, start);
  } else {
    start = Math.max(0, evs.length - state.renderLimit);
    const nodes: HTMLElement[] = [];
    if (start > 0) {
      const more = el('button', 'btn more', `Show earlier (${start} hidden)`);
      more.type = 'button';
      more.addEventListener('click', () => renderEvents({ earlier: true }));
      nodes.push(more);
    }
    nodes.push(...evs.slice(start).map(eventNode));
    // Opening or switching a session rebuilds the log: that must not be read aloud (local #245).
    quietRebuild(pane, () => pane.replaceChildren(...nodes));
  }
  state.renderedCount = evs.length - start;
  state.renderedFirstSeq = evs[start]?.sequence;
  if (stick || opening) {
    pane.scrollTop = pane.scrollHeight;
    state.unseenEvents = 0;
  } else {
    if (anchor) restoreAnchor(pane, anchor);
    state.unseenEvents += appended;
  }
  syncJump();
  renderOutcome();
}

export async function refreshEvents(full = false): Promise<void> {
  const id = state.selectedId;
  if (!id) return;
  if (state.eventsInFlight && !full) { state.eventsDirty = true; return; }
  const epoch = state.selectionEpoch;
  state.eventsInFlight = true;
  try {
    const since = full ? undefined : maxSequence(state.events);
    const fresh = await api.getConversation(id, since);
    if (epoch !== state.selectionEpoch) return;
    const merged = mergeEvents(state.events, fresh);
    if (merged !== state.events || full) {
      // A changed earlier event forces a full re-render.
      const rewrote = fresh.some((e) => e.sequence <= (maxSequence(state.events) ?? -1));
      state.events = merged;
      if (rewrote) state.renderedCount = 0;
      renderEvents();
    }
  } catch (e) {
    if (epoch === state.selectionEpoch && full) toast(`Could not load conversation: ${String(e)}`);
  } finally {
    state.eventsInFlight = false;
    if (state.eventsDirty) { state.eventsDirty = false; void refreshEvents(); }
  }
}
