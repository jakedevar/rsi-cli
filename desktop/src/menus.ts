// Thin Tauri adapter for native menus. The models come from registry.ts
// (appMenuModel / contextMenuModel, unit-tested); the Rust side
// (src-tauri/src/menu.rs) validates and builds them and sends the chosen item
// id back as a `menu-action` event, which runs through commands.ts dispatch.
// Outside Tauri (a browser preview) nothing here runs and the browser's own
// context menu stays.
import { api, inTauri } from './api.js';
import { dispatch, setContextEvent, snapshot } from './commands.js';
import { select } from './sessions.js';
import { appMenuModel, contextMenuModel, menuState, parseMenuId } from './registry.js';
import { onUiChange } from './state.js';
import { currentPlatform } from './vimkeys.js';

export const MENU_EVENT = 'menu-action';

interface TauriEvents {
  event?: { listen(name: string, cb: (e: { payload: unknown }) => void): Promise<() => void> };
}

let lastState = '';
let syncQueued = false;

function syncMenuState(): void {
  syncQueued = false;
  const items = menuState(appMenuModel(currentPlatform(), snapshot()));
  const sig = JSON.stringify(items);
  if (sig === lastState) return;
  lastState = sig;
  api.syncMenuState(items).catch(() => { lastState = ''; });
}

function onMenuAction(payload: unknown): void {
  const id = (payload as { id?: unknown } | null)?.id;
  const m = typeof id === 'string' ? parseMenuId(id) : null;
  if (m) dispatch(m.action, { source: m.source });
}

function popup(kind: 'session-row' | 'event'): void {
  api.popupContextMenu(contextMenuModel(kind, snapshot())).catch(() => undefined);
}

export async function initMenus(): Promise<void> {
  if (!inTauri()) return;
  const model = appMenuModel(currentPlatform(), snapshot());
  try {
    await api.setAppMenu(model);
    lastState = JSON.stringify(menuState(model));
  } catch (e) {
    console.warn('app menu not installed', e);
  }
  const listen = (globalThis as unknown as { __TAURI__?: TauriEvents }).__TAURI__?.event?.listen;
  if (listen) void listen(MENU_EVENT, (e) => onMenuAction(e.payload));
  onUiChange(() => {
    if (syncQueued) return;
    syncQueued = true;
    window.setTimeout(syncMenuState, 50);
  });
  // Right-click a session row: open it (session actions act on the open
  // session), then show its actions.
  document.getElementById('session-list')?.addEventListener('contextmenu', (ev) => {
    const li = (ev.target as HTMLElement).closest<HTMLElement>('.session-item');
    if (!li?.dataset.id) return;
    ev.preventDefault();
    setContextEvent(null);
    select(li.dataset.id);
    popup('session-row');
  });
  // Right-click a conversation event. A text selection keeps the webview's
  // own menu (Copy, Look Up ...).
  document.getElementById('events')?.addEventListener('contextmenu', (ev) => {
    const box = (ev.target as HTMLElement).closest<HTMLElement>('.ev');
    const sel = window.getSelection();
    if (!box || (sel && !sel.isCollapsed && box.contains(sel.anchorNode))) return;
    ev.preventDefault();
    setContextEvent(box);
    popup('event');
  });
}
