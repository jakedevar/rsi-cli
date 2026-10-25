// OS notifications when a session needs me (local #294): the DOM side of notify-model.ts. The two switches live in
// localStorage (no daemon setting); the post goes through the Rust notify_session command; a click on a notification
// comes back as the `notification-open` event and opens that session.
import { api, inTauri, onTauriEvent } from './api.js';
import type { Session } from './api.js';
import {
  type Notice, type NotifyPermission, type NotifyPrefs, NOTIFY_PREFS_KEY, NOTIFY_TEST_BODY, NOTIFY_TEST_TITLE,
  createNotifier, parseNotifyPermission, parseNotifyPrefs, serializeNotifyPrefs,
} from './notify-model.js';

function storage(): Storage | undefined {
  try { return window.localStorage; } catch { return undefined; }
}

let prefs: NotifyPrefs | null = null;

export function notifyPrefs(): NotifyPrefs {
  if (!prefs) {
    let raw: string | null = null;
    try { raw = storage()?.getItem(NOTIFY_PREFS_KEY) ?? null; } catch { raw = null; }
    prefs = parseNotifyPrefs(raw);
  }
  return prefs;
}

/** Turn one switch on or off; it applies from the next refresh and is remembered. */
export function setNotifyPref(which: keyof NotifyPrefs, on: boolean): void {
  prefs = { ...notifyPrefs(), [which]: on };
  try { storage()?.setItem(NOTIFY_PREFS_KEY, serializeNotifyPrefs(prefs)); } catch { /* no storage: this window only */ }
}

/** The window is in front and has the keyboard. */
function windowFocused(): boolean {
  try { return document.visibilityState === 'visible' && document.hasFocus(); } catch { return false; }
}

async function post(n: Notice): Promise<void> {
  if (!inTauri()) return; // a browser page has no OS to tell
  await api.notifySession(n.sessionId, n.title, n.body);
}

const notifier = createNotifier({
  prefs: notifyPrefs,
  focused: windowFocused,
  post,
  failed: (e) => console.warn('notification not shown', e),
});

/** Called after every session refresh with the open session's id. */
export function notifyAttention(all: readonly Session[], openId: string | undefined): void {
  notifier.update(all, openId);
}

/** Settings › Send a test notification: the one you asked for, so it ignores the switches and the focus. */
export function sendTestNotification(): Promise<void> {
  return post({ sessionId: null, kind: 'test', title: NOTIFY_TEST_TITLE, body: NOTIFY_TEST_BODY });
}

/** A click on a notification opens its session (the Rust side already brought the window forward). */
export function initNotify(open: (sessionId: string) => void): void {
  onTauriEvent('notification-open', (payload) => {
    if (typeof payload === 'string' && payload) open(payload);
  });
}

let permission: NotifyPermission = 'unknown';

/** What macOS last said about notifications for RSI (unknown until read, and outside the desktop app on macOS). */
export function notifyPermission(): NotifyPermission {
  return permission;
}

/** Ask macOS again (read only: nothing is posted or requested); true when the answer changed, so Settings redraws. */
export async function refreshNotifyPermission(): Promise<boolean> {
  let next: NotifyPermission = 'unknown';
  if (inTauri()) {
    try { next = parseNotifyPermission(await api.notifyPermission()); } catch { next = 'unknown'; }
  }
  const changed = next !== permission;
  permission = next;
  return changed;
}

/** Settings › Open Notification settings: System Settings › Notifications. */
export function openNotificationSettings(): Promise<void> {
  return api.openNotificationSettings();
}
