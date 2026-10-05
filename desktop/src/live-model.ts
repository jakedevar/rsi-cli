// Pure helpers for live (push) updates. No DOM, no Tauri: unit-testable.

/** Payload of the backend's `daemon-event` (bounded; no daemon text). */
export interface LiveEvent {
  event_type: string;
  session_id?: string | null;
}

/** What a daemon event means for the UI. */
export interface Refresh {
  sessions: boolean;
  conversation: boolean;
}

const NONE: Refresh = { sessions: false, conversation: false };

/** Events that change the session list (status, title, membership...). */
const LIST_EVENTS = new Set([
  'session_created', 'session_status_changed', 'session_deleted', 'session_archived',
  'session_unarchived', 'session_metadata_changed', 'session_summary_updated',
  'session_question_raised', 'session_retrying', 'session_stalled', 'session_classified',
  'session_reconciled', 'child_spawned',
]);

/** Synthetic/overflow events after which nothing can be assumed current. */
const RESYNC_EVENTS = new Set(['subscription_reset', 'oversize']);

/** Decide what to refresh for one event, given the selected session. */
export function classifyEvent(ev: LiveEvent, selectedId: string | null): Refresh {
  const type = ev.event_type;
  if (RESYNC_EVENTS.has(type)) return { sessions: true, conversation: selectedId !== null };
  const mine = selectedId !== null && ev.session_id === selectedId;
  if (type === 'conversation_event') return { sessions: false, conversation: mine };
  if (LIST_EVENTS.has(type)) return { sessions: true, conversation: mine };
  return NONE;
}

export function mergeRefresh(a: Refresh, b: Refresh): Refresh {
  return { sessions: a.sessions || b.sessions, conversation: a.conversation || b.conversation };
}

export const NO_REFRESH: Refresh = NONE;

export interface PollIntervals {
  sessionsMs: number;
  eventsMs: number;
}

/** Slow fallback while the push link is up; fast polling while it is down. */
export const SLOW_POLL: PollIntervals = { sessionsMs: 15_000, eventsMs: 10_000 };
export const FAST_POLL: PollIntervals = { sessionsMs: 3_000, eventsMs: 1_500 };

export function pollIntervals(linkUp: boolean): PollIntervals {
  return linkUp ? SLOW_POLL : FAST_POLL;
}

/** Narrow an unknown Tauri event payload to a [`LiveEvent`]. */
export function parseLiveEvent(payload: unknown): LiveEvent | null {
  if (typeof payload !== 'object' || payload === null) return null;
  const p = payload as Record<string, unknown>;
  if (typeof p['event_type'] !== 'string') return null;
  const sid = p['session_id'];
  return { event_type: p['event_type'], session_id: typeof sid === 'string' ? sid : null };
}

/** Narrow an unknown Tauri `daemon-link` payload to the link state. */
export function parseLink(payload: unknown): boolean | null {
  if (typeof payload !== 'object' || payload === null) return null;
  const up = (payload as Record<string, unknown>)['up'];
  return typeof up === 'boolean' ? up : null;
}
