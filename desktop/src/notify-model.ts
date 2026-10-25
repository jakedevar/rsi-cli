// When to post an OS notification (local #294). No DOM: the page hands in the session list, whether
// its window has focus and which session is open, and gets back what to post. Unit-tested under tests/.
import type { Session } from './api.js';
import { needsAttention, pendingQuestions, questionKey, waitingAnnouncement } from './approvals-model.js';
import { sessionLabel } from './model.js';

/** Two switches, kept on this computer (no daemon setting): "needs me" on, "finished" off. */
export interface NotifyPrefs {
  needsMe: boolean;
  finished: boolean;
}

export const NOTIFY_PREFS_KEY = 'rsi.notify.v1';
export const DEFAULT_NOTIFY_PREFS: NotifyPrefs = { needsMe: true, finished: false };

export function parseNotifyPrefs(raw: string | null | undefined): NotifyPrefs {
  if (!raw) return { ...DEFAULT_NOTIFY_PREFS };
  try {
    const v = JSON.parse(raw) as Partial<NotifyPrefs> | null;
    return {
      needsMe: typeof v?.needsMe === 'boolean' ? v.needsMe : DEFAULT_NOTIFY_PREFS.needsMe,
      finished: typeof v?.finished === 'boolean' ? v.finished : DEFAULT_NOTIFY_PREFS.finished,
    };
  } catch {
    return { ...DEFAULT_NOTIFY_PREFS };
  }
}

export function serializeNotifyPrefs(p: NotifyPrefs): string {
  return JSON.stringify({ needsMe: p.needsMe, finished: p.finished });
}

/** Words for the Settings rows and the test notification. */
export const NOTIFY_NEEDS_LABEL = 'Notify me when a session needs me';
export const NOTIFY_FINISHED_LABEL = 'Notify me when a session finishes';
export const NOTIFY_TEST_LABEL = 'Send a test notification';
/** What macOS says about notifications for RSI (src-tauri/src/notify.rs, local #300). `unknown`: not macOS, not an app bundle, or no answer. */
export type NotifyPermission = 'granted' | 'denied' | 'not_determined' | 'unknown';

export function parseNotifyPermission(raw: unknown): NotifyPermission {
  return raw === 'granted' || raw === 'denied' || raw === 'not_determined' ? raw : 'unknown';
}

/** The standing hint, kept where the state is not known (and until it is read). */
export const NOTIFY_PERMISSION_HELP = 'macOS asks the first time a notification is shown. If you chose Don\'t Allow, or nothing appears, open System Settings › Notifications, pick RSI and turn on Allow Notifications. The Dock badge shows how many sessions need you either way.';
/** The one line shown when macOS has refused (the hint above is dropped then; granted and not yet asked say nothing). */
export const NOTIFY_DENIED_LABEL = 'macOS is blocking notifications from RSI';
export const NOTIFY_DENIED_SUMMARY = 'You chose Don\'t Allow, or turned notifications off, so nothing appears outside this window. To get them, open System Settings › Notifications, pick RSI and turn on Allow Notifications. The Dock badge still shows how many sessions need you.';
export const NOTIFY_OPEN_SETTINGS_BUTTON = 'Open Notification settings ▸';

/** The permission sentence a row ends with: the standing hint while unknown, nothing once macOS has said granted or not asked yet. */
export function notifyPermissionHint(p: NotifyPermission): string {
  return p === 'unknown' ? ' ' + NOTIFY_PERMISSION_HELP : '';
}

export const NOTIFY_TEST_TITLE = 'RSI notifications are on';
export const NOTIFY_TEST_BODY = 'This is how a session that needs you will appear.';

/** What the page asks the OS to show. */
export interface Notice {
  /** The session a click opens; null for the Settings test. */
  sessionId: string | null;
  kind: 'needs' | 'finished' | 'test';
  title: string;
  body: string;
}

/** What the last refresh saw, so a change notifies once. */
export interface NotifyTracker {
  /** session id -> the request it was last seen waiting on (status + questions); null until the first refresh seeds it. */
  needs: ReadonlyMap<string, string> | null;
  /** Ids last seen alive (running or waiting); a finish is only news for one of these. */
  live: ReadonlySet<string>;
}

export const FRESH_TRACKER: NotifyTracker = { needs: null, live: new Set() };

export interface NotifyContext {
  prefs: NotifyPrefs;
  /** The app window is in front and has the keyboard. */
  focused: boolean;
  /** The session open in the conversation view. */
  openId: string | undefined;
}

/** More waiting sessions than this at once become one summary notification. */
export const MAX_SEPARATE = 3;
const BODY_CHARS = 140;

const ALIVE = new Set(['Starting', 'Running', 'WaitingApproval']);

function oneLine(text: string, max = BODY_CHARS): string {
  const flat = text.replace(/\s+/g, ' ').trim();
  return flat.length > max ? `${flat.slice(0, max - 1).trimEnd()}…` : flat;
}

/** What a waiting session asks, as one line: its first question, or that it wants an approval. */
export function needsBody(s: Pick<Session, 'status' | 'pending_question'>): string {
  const q = pendingQuestions(s)[0];
  const text = q ? (q.question || q.header || '') : '';
  return oneLine(text) || 'Waiting for your approval';
}

export function finishedBody(s: Pick<Session, 'status'>): string {
  return s.status === 'Failed' ? 'Stopped with an error' : 'Finished';
}

/**
 * Compare this refresh with the last one and say what to post. A session that was already waiting at the
 * first refresh, or whose request is the one already announced, posts nothing; a session answered and then
 * asking again (a different request) posts again. Nothing is posted for a session the person is looking at.
 * A session missing from the list keeps its mark, so a refetch that briefly drops it does not repeat it.
 */
export function planNotices(
  tracker: NotifyTracker, all: readonly Session[], ctx: NotifyContext,
): { tracker: NotifyTracker; notices: Notice[] } {
  const needs = new Map(tracker.needs ?? []);
  const live = new Set(tracker.live);
  const seeding = tracker.needs === null;
  const needNotices: Notice[] = [];
  const doneNotices: Notice[] = [];
  const watching = (s: Session): boolean => ctx.focused && ctx.openId === s.id;
  for (const s of all) {
    if (needsAttention(s)) {
      const key = pendingQuestions(s).length > 0 ? questionKey(s) : 'approval';
      const was = needs.get(s.id);
      needs.set(s.id, key);
      live.add(s.id);
      if (!seeding && was !== key && ctx.prefs.needsMe && !watching(s)) {
        needNotices.push({ sessionId: s.id, kind: 'needs', title: sessionLabel(s), body: needsBody(s) });
      }
      continue;
    }
    needs.delete(s.id);
    if (ALIVE.has(s.status)) { live.add(s.id); continue; }
    const wasAlive = live.delete(s.id);
    if (!seeding && wasAlive && (s.status === 'Completed' || s.status === 'Failed') && ctx.prefs.finished && !watching(s)) {
      doneNotices.push({ sessionId: s.id, kind: 'finished', title: sessionLabel(s), body: finishedBody(s) });
    }
  }
  return { tracker: { needs, live }, notices: [...summarize(needNotices), ...summarize(doneNotices)] };
}

/** A burst of sessions becomes one notification naming them, which opens the first. */
function summarize(list: Notice[]): Notice[] {
  const first = list[0];
  if (!first || list.length <= MAX_SEPARATE) return list;
  const names = list.map((n) => n.title);
  if (first.kind === 'needs') {
    return [{ sessionId: first.sessionId, kind: 'needs', title: `${list.length} sessions need you`, body: oneLine(waitingAnnouncement(names)) }];
  }
  const shown = names.slice(0, 3).join(', ');
  const more = names.length > 3 ? ` and ${names.length - 3} more` : '';
  return [{ sessionId: first.sessionId, kind: 'finished', title: `${list.length} sessions finished`, body: oneLine(`${shown}${more}`) }];
}

/** The things the notifier needs from the page; a test hands in stubs. */
export interface NotifyEnv {
  prefs: () => NotifyPrefs;
  focused: () => boolean;
  /** Post one notification through the Rust command. */
  post: (n: Notice) => Promise<void>;
  /** A post that failed (the page logs it; there is no retry). */
  failed?: (e: unknown) => void;
}

/** Remember what was announced between refreshes and post what is new. */
export function createNotifier(env: NotifyEnv): { update: (all: readonly Session[], openId: string | undefined) => Notice[] } {
  let tracker = FRESH_TRACKER;
  return {
    update(all, openId) {
      const r = planNotices(tracker, all, { prefs: env.prefs(), focused: env.focused(), openId });
      tracker = r.tracker;
      for (const n of r.notices) env.post(n).catch((e) => env.failed?.(e));
      return r.notices;
    },
  };
}
