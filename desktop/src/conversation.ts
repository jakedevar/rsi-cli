// Tool/thinking/system text is rendered with textContent. User and assistant
// messages go through markdown.ts (Rust-sanitized HTML, the only innerHTML path).
// Conversation pane: event rendering and incremental polling.
import { api, type ConversationEvent } from './api.js';
import { $, el, toast } from './dom.js';
import { markdownBody } from './markdown.js';
import { splitFold } from './mdmodel.js';
import { FOLD_CHARS, elideBlobs, eventKind, maxSequence, mergeEvents } from './model.js';
import { RENDER_PAGE, state } from './state.js';

export function eventNode(e: ConversationEvent): HTMLElement {
  const kind = eventKind(e);
  const box = el('div', `ev ev-${kind}`);
  const when = new Date(e.created_at).toLocaleTimeString();
  if (kind === 'tool' || kind === 'result' || kind === 'thinking') {
    const d = el('details');
    const label = kind === 'tool' ? `▸ ${e.tool_name ?? 'tool'}`
      : kind === 'result' ? `◂ result${e.tool_name ? ` (${e.tool_name})` : ''}`
      : '… thinking';
    const firstLine = elideBlobs(e.content).split('\n', 1)[0]?.slice(0, 100) ?? '';
    d.append(el('summary', undefined, `${when}  ${label}  ${firstLine}`));
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

export function renderEvents(): void {
  const pane = $('events');
  const stick = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 80;
  const start = Math.max(0, state.events.length - state.renderLimit);
  const shown = state.events.slice(start);
  const firstSeq = shown[0]?.sequence;
  const canAppend = state.renderedCount > 0 && firstSeq === state.renderedFirstSeq &&
    shown.length >= state.renderedCount;
  if (canAppend) {
    for (const e of shown.slice(state.renderedCount)) pane.append(eventNode(e));
  } else {
    const nodes: HTMLElement[] = [];
    if (start > 0) {
      const more = el('button', 'btn more', `Show earlier (${start} hidden)`);
      more.type = 'button';
      more.addEventListener('click', () => {
        state.renderLimit += RENDER_PAGE;
        state.renderedCount = 0;
        renderEvents();
      });
      nodes.push(more);
    }
    nodes.push(...shown.map(eventNode));
    pane.replaceChildren(...nodes);
  }
  state.renderedCount = shown.length;
  state.renderedFirstSeq = firstSeq;
  if (stick || !canAppend) pane.scrollTop = pane.scrollHeight;
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
