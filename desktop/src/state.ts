// Shared UI state. View modules read and mutate it; main.ts owns timers.
import type { ConversationEvent, Project, Session, SessionLabel } from './api.js';
import type { SortOrderId } from './listmodel.js';

export const RENDER_PAGE = 400;

export const state = {
  sessions: [] as Session[],
  archived: [] as Session[],
  view: 'active' as 'active' | 'archived',
  /** The Active view's zone (G2, gs / gj): Main, or the root sessions scheduled jobs launched. */
  zone: 'main' as 'main' | 'jobs',
  archivedFetchedAt: 0,
  projects: [] as Project[],
  labels: [] as SessionLabel[],
  /** Session-list sort order (the TUI's <Space>s picker), kept in local storage. */
  sortOrder: 'stalest' as SortOrderId,
  selectedId: null as string | null,
  /**
   * The open session after it left the list it came from (archived from here, or by the TUI or an agent), as
   * rsid last reported it. The pane shows it read-only instead of keeping its last drawing (local #309).
   */
  departed: null as Session | null,
  /** Keyboard cursor in the session list (vim keys); Enter opens it. */
  cursorId: null as string | null,
  events: [] as ConversationEvent[],
  renderLimit: RENDER_PAGE,
  renderedCount: 0,
  renderedFirstSeq: undefined as number | undefined,
  /** Events that arrived while the reader was scrolled up (the "Jump to latest" count, local #306). */
  unseenEvents: 0,
  hideTools: false,
  sessionsInFlight: false,
  eventsInFlight: false,
  /** A push refresh arrived while one was running: run once more after. */
  sessionsDirty: false,
  eventsDirty: false,
  /** Bumped on every selection so stale conversation replies are dropped. */
  selectionEpoch: 0,
  /** The daemon link is down (unreachable or backend error): the open session's state is last-known (local #266). */
  daemonDown: false,
};

export function currentSessions(): Session[] {
  return state.view === 'archived' ? state.archived : state.sessions;
}

export function selected(): Session | undefined {
  const found = currentSessions().find((s) => s.id === state.selectedId);
  if (found) return found;
  // Only the Active view has a session that left it; the Archived view lists what it lists.
  return state.view === 'active' && state.departed?.id === state.selectedId ? state.departed : undefined;
}

/** True while the open session is the one that left the list (see `state.departed`). */
export function isDeparted(s: Session | undefined): boolean {
  return !!s && state.departed === s;
}

/** Archived sessions are history: no composer, no interrupt. A session that left the list is read-only too. */
export function isReadOnly(s: Session | undefined): boolean {
  return state.view === 'archived' || s?.status === 'Archived' || isDeparted(s);
}

const uiListeners: Array<() => void> = [];

/** Run `cb` after anything the actions' availability reads may have changed. */
export function onUiChange(cb: () => void): void {
  uiListeners.push(cb);
}

/** Views call this after they re-render (session list, header, controls). */
export function uiChanged(): void {
  for (const cb of uiListeners) cb();
}

export function projectName(id: string | null | undefined): string | undefined {
  return id ? state.projects.find((p) => p.id === id)?.name : undefined;
}
