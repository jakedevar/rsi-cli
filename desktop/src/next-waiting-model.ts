// The step after an answer: offer the next session that waits for the operator (local #312). Pure, no DOM.
import type { Session } from './api.js';
import { sessionLabel } from './model.js';

export const NONE_WAITING = 'No more sessions waiting';
export const NEXT_WAITING_LABEL = 'Next waiting';

export interface NextWaiting {
  /** The session `]a` would open next: the one after `answered` in list order, wrapping; null when none waits. */
  next: Session | null;
  /** Waiting sessions other than `answered` (its own row can still show the old question until the refresh lands). */
  left: number;
}

/** `queue` is the waiting sessions in list order (listmodel.attentionQueue); `answeredId` is the session just answered. */
export function nextWaiting(queue: readonly Session[], answeredId: string): NextWaiting {
  const others = queue.filter((s) => s.id !== answeredId);
  if (others.length === 0) return { next: null, left: 0 };
  const at = queue.findIndex((s) => s.id === answeredId);
  // After the answered row; with the answered row not in the queue (it already cleared), the first waiting row.
  const next = at < 0 ? others[0]! : queue.slice(at + 1).concat(queue.slice(0, at)).find((s) => s.id !== answeredId)!;
  return { next, left: others.length };
}

/** The button text: "Next waiting (2)". */
export function nextWaitingButton(left: number): string {
  return `${NEXT_WAITING_LABEL} (${left})`;
}

/** What the button opens, for its tooltip and screen-reader name. */
export function nextWaitingTitle(next: Session): string {
  return `Open ${sessionLabel(next)} and its question`;
}
