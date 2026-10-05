// Live updates: subscribe to the backend's `daemon-event` / `daemon-link`
// Tauri events, debounce-refresh what changed, and keep polling only as a
// fallback (slow while the push link is up, fast while it is down).
import {
  NO_REFRESH, classifyEvent, mergeRefresh, parseLink, parseLiveEvent, pollIntervals,
  type Refresh,
} from './live-model.js';

const DEBOUNCE_MS = 250;

export interface LiveHooks {
  refreshSessions(): Promise<void>;
  refreshEvents(): Promise<void>;
  selectedId(): string | null;
  /** Called when the push link goes up or down. */
  onLink?(up: boolean): void;
}

interface TauriLive {
  core?: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
  event?: {
    listen(name: string, cb: (e: { payload: unknown }) => void): Promise<() => void>;
  };
}

export interface Live {
  linkUp(): boolean;
}

export function startLive(hooks: LiveHooks): Live {
  const tauri = (globalThis as unknown as { __TAURI__?: TauriLive }).__TAURI__;
  let up = false;
  let pending: Refresh = NO_REFRESH;
  let timer: number | undefined;

  const flush = (): void => {
    timer = undefined;
    const todo = pending;
    pending = NO_REFRESH;
    if (todo.sessions) void hooks.refreshSessions();
    if (todo.conversation) void hooks.refreshEvents();
  };
  const schedule = (r: Refresh): void => {
    pending = mergeRefresh(pending, r);
    if (!pending.sessions && !pending.conversation) return;
    if (timer === undefined) timer = window.setTimeout(flush, DEBOUNCE_MS);
  };
  const setLink = (next: boolean): void => {
    if (next === up) return;
    up = next;
    hooks.onLink?.(up);
    // Catch up on anything missed while the link was down (and on connect).
    if (up) schedule({ sessions: true, conversation: hooks.selectedId() !== null });
  };

  // Fallback polling; intervals re-read the link state on every tick.
  const poll = (which: 'sessionsMs' | 'eventsMs', run: () => Promise<void>): void => {
    const tick = (): void => {
      void run();
      window.setTimeout(tick, pollIntervals(up)[which]);
    };
    window.setTimeout(tick, pollIntervals(up)[which]);
  };
  poll('sessionsMs', () => hooks.refreshSessions());
  poll('eventsMs', () => hooks.refreshEvents());

  const listen = tauri?.event?.listen;
  if (listen) {
    void listen('daemon-event', (e) => {
      const ev = parseLiveEvent(e.payload);
      if (ev) schedule(classifyEvent(ev, hooks.selectedId()));
    });
    void listen('daemon-link', (e) => {
      const next = parseLink(e.payload);
      if (next !== null) setLink(next);
    });
    // The first link event may have fired before the listeners existed.
    void tauri?.core?.invoke<boolean>('live_link').then(setLink).catch(() => undefined);
  }
  return { linkUp: () => up };
}
