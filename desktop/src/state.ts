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
  /** Keyboard cursor in the session list (vim keys); Enter opens it. */
  cursorId: null as string | null,
  events: [] as ConversationEvent[],
  renderLimit: RENDER_PAGE,
  renderedCount: 0,
  renderedFirstSeq: undefined as number | undefined,
  hideTools: false,
  sessionsInFlight: false,
  eventsInFlight: false,
  /** A push refresh arrived while one was running: run once more after. */
  sessionsDirty: false,
  eventsDirty: false,
  /** Bumped on every selection so stale conversation replies are dropped. */
  selectionEpoch: 0,
};

export function currentSessions(): Session[] {
  return state.view === 'archived' ? state.archived : state.sessions;
}

export function selected(): Session | undefined {
  return currentSessions().find((s) => s.id === state.selectedId);
}

/** Archived sessions are history: no composer, no interrupt. */
export function isReadOnly(s: Session | undefined): boolean {
  return state.view === 'archived' || s?.status === 'Archived';
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
