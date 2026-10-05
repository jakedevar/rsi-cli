// Shared UI state. View modules read and mutate it; main.ts owns timers.
import type { ConversationEvent, Project, Session } from './api.js';

export const RENDER_PAGE = 400;

export const state = {
  sessions: [] as Session[],
  archived: [] as Session[],
  view: 'active' as 'active' | 'archived',
  archivedFetchedAt: 0,
  projects: [] as Project[],
  selectedId: null as string | null,
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

export function projectName(id: string | null | undefined): string | undefined {
  return id ? state.projects.find((p) => p.id === id)?.name : undefined;
}
