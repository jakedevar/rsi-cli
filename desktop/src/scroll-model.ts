// Reading position of the conversation pane (local #306). Pure rules, no DOM: conversation.ts applies them.
// A reader who scrolled up keeps their place when events arrive, when "Show earlier" prepends a page and
// when the window is trimmed; a reader at the bottom follows the tail.
import { RENDER_PAGE } from './state.js';

/** Within this many pixels of the end the reader counts as "at the bottom" and follows new events. */
export const NEAR_BOTTOM_PX = 80;
/** Events kept above the reader's event when a trim has to stop short of the usual limit. */
export const ANCHOR_CONTEXT = 50;
/** The window may grow this far past the render limit before the oldest events are trimmed off the top. */
export const TRIM_SLACK = RENDER_PAGE;

export interface Scrollish { scrollTop: number; scrollHeight: number; clientHeight: number }

export const atBottom = (p: Scrollish, slop = NEAR_BOTTOM_PX): boolean =>
  p.scrollHeight - p.scrollTop - p.clientHeight < slop;

/** The event at the top of the viewport and how far below the pane's top edge it sits. */
export interface Anchor { seq: number; offset: number }

/** The scrollTop that puts the anchored event back where it was, given where it is now. */
export const restoredScrollTop = (scrollTop: number, nodeTop: number, paneTop: number, offset: number): number =>
  scrollTop + (nodeTop - paneTop) - offset;

export const jumpLabel = (unseen: number): string =>
  `Jump to latest (${unseen} new)`;

export interface WindowPlan {
  /** Index into the events of the first one to render. */
  start: number;
  /** The first rendered event is no longer the DOM's first: older events are added (negative) or dropped (positive). */
  shift: number;
}

/**
 * Where the rendered window starts after a change.
 * - `earlier`: one more page above the current start.
 * - Otherwise the start stays put (appending never rebuilds); only when the window outgrew the limit by
 *   more than TRIM_SLACK is it trimmed to `limit`, and never past (ANCHOR_CONTEXT before) the event the reader is looking at.
 * `domIdx` is the index of the DOM's first event, `anchorIdx` the index of the anchored event or null.
 */
export function planWindow(total: number, domIdx: number, limit: number, opts: { earlier: boolean; anchorIdx: number | null; atBottom: boolean }): WindowPlan {
  let start = domIdx;
  if (opts.earlier) start = Math.max(0, domIdx - RENDER_PAGE);
  else if (total - domIdx > limit + TRIM_SLACK) {
    const trimmed = total - limit;
    // A reader further up keeps their event and a little context: the trim stops above it, never below.
    start = opts.atBottom || opts.anchorIdx === null ? trimmed : Math.max(domIdx, Math.min(trimmed, opts.anchorIdx - ANCHOR_CONTEXT));
  }
  return { start, shift: start - domIdx };
}
