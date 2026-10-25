// Pure view-model helpers (no DOM), unit-tested under tests/.
import type { ConversationEvent, Session } from './api.js';
import { needsAttention } from './approvals-model.js';

export const ACTIVE_STATUSES = new Set(['Starting', 'Running', 'WaitingApproval']);
export const HIDDEN_BY_DEFAULT = new Set(['Archived', 'Deleted']);

export function isActive(s: Pick<Session, 'status'>): boolean {
  return ACTIVE_STATUSES.has(s.status);
}

/** A session status in plain words (the raw enum stays in classes and tooltips). */
export function statusLabel(status: string): string {
  return status === 'WaitingApproval' ? 'Waiting for you' : status;
}

/** Lists longer than this re-render once per pause in typing; shorter ones on every key. */
export const FILTER_DEBOUNCE_ABOVE = 400;
const FILTER_DEBOUNCE_MS = 120;

/** How long the filter box waits after a key before re-rendering a list of `count` sessions. */
export function filterDelayMs(count: number): number {
  return count > FILTER_DEBOUNCE_ABOVE ? FILTER_DEBOUNCE_MS : 0;
}

/**
 * Render now, or wait for a pause in typing. A short list always renders at once; on a long one the first
 * key after a pause renders at once (no added latency) and keys typed while a render is fresh or a wait
 * is pending coalesce into one trailing render.
 */
export function filterRenderPlan(count: number, sinceLastRenderMs: number, waiting: boolean): 'now' | 'later' {
  const delay = filterDelayMs(count);
  return delay === 0 || (!waiting && sinceLastRenderMs >= delay) ? 'now' : 'later';
}

/** Matches of the current filter that the other filters hide (see tree.ts countHiddenMatches). */
export interface HiddenMatches {
  /** Completed sessions hidden because Show completed is off. */
  finished: number;
  /** Archived sessions (they list on the Archived tab). */
  archived: number;
  /** Sessions in other projects, when a project is chosen. */
  otherProjects: number;
}

export type EmptyActionId = 'show-finished' | 'show-archived' | 'clear-filter' | 'all-projects';

export interface EmptyListInput {
  view: 'active' | 'archived';
  /** The Active view's zone: Main, or the sessions scheduled jobs launched (Jobs). */
  zone: 'main' | 'jobs';
  /** Sessions this tab could list with no filter on (any project, finished or not). */
  total: number;
  /** The filter box text. */
  text: string;
  projectOn: boolean;
  finishedHidden: boolean;
  hidden: HiddenMatches;
}

export interface EmptyListView {
  text: string;
  actions: { id: EmptyActionId; label: string }[];
}

const NO_HIDDEN: HiddenMatches = { finished: 0, archived: 0, otherProjects: 0 };

function quoted(text: string): string {
  const t = text.trim().replace(/\s+/g, ' ');
  return `\u201c${t.length > 40 ? `${t.slice(0, 39)}\u2026` : t}\u201d`;
}

function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}

/**
 * The session list's empty row: what this tab is for and, when a filter hides something, which
 * filters are on and how many matches each hides. It names only the filters that are on, so it
 * never tells the user to "choose All projects" when All projects is chosen (local #272).
 */
export function emptyListView(i: EmptyListInput): EmptyListView {
  const h = i.hidden ?? NO_HIDDEN;
  const text = i.text.trim();
  const actions: EmptyListView['actions'] = [];
  const subject = i.view === 'archived' ? 'archived sessions' : i.zone === 'jobs' ? 'scheduled-job sessions' : 'sessions';
  if (i.total === 0) {
    return {
      text: i.view === 'archived' ? 'No archived sessions.'
        : i.zone === 'jobs' ? 'No scheduled-job sessions yet. Sessions that scheduled jobs launch appear here.'
          : 'No sessions yet. Click New session to start one.',
      actions,
    };
  }
  const lead = text ? `No ${subject} match ${quoted(text)}.` : `No ${subject} to show.`;
  const hiddenParts: string[] = [];
  if (i.view === 'active' && i.finishedHidden && h.finished > 0) {
    hiddenParts.push(`${plural(h.finished, 'completed session')} hidden`);
    actions.push({ id: 'show-finished', label: 'Show completed' });
  }
  if (i.view === 'active' && h.archived > 0) {
    hiddenParts.push(`${plural(h.archived, 'archived session')}`);
    actions.push({ id: 'show-archived', label: 'Show archived' });
  }
  if (i.projectOn && h.otherProjects > 0) {
    hiddenParts.push(`${plural(h.otherProjects, 'match', 'matches')} in other projects`);
    actions.push({ id: 'all-projects', label: 'All projects' });
  }
  if (text) actions.push({ id: 'clear-filter', label: 'Clear filter' });
  if (hiddenParts.length > 0) return { text: `${lead} Also found: ${hiddenParts.join(', ')}.`, actions };
  // Nothing hidden to offer: name the filters that are on.
  const on: string[] = [];
  if (text) on.push('clear the filter');
  if (i.projectOn) on.push('choose All projects');
  if (i.view === 'active' && i.finishedHidden) on.push('tick Show completed');
  const tail = on.length === 0 ? '' : ` ${on.length === 1 ? on[0]! : `${on.slice(0, -1).join(', ')} or ${on[on.length - 1]}`}.`;
  return { text: `${lead}${tail ? tail.replace(/^ ./, (c) => c.toUpperCase()) : ''}`, actions };
}

/** localStorage key for the Show completed choice (a desktop view preference, like the theme). */
export const SHOW_FINISHED_KEY = 'rsi.desktop.showFinished';

/** Finished sessions show by default, as in the TUI; only an explicit "off" hides them. */
export function parseShowFinished(raw: string | null | undefined): boolean {
  return raw !== 'off';
}

export function serializeShowFinished(on: boolean): string {
  return on ? 'on' : 'off';
}

/**
 * The finished-session part of the list filter. Active, Interrupted and Failed
 * sessions always show; Completed ones show unless the user hid them, and the
 * session that is open (`keepId`) stays listed even if it just finished.
 */
export function passesFinished(s: Pick<Session, 'id' | 'status'>, showFinished: boolean, keepId?: string | null): boolean {
  return showFinished || isActive(s) || s.status === 'Interrupted' || s.status === 'Failed' || (!!keepId && s.id === keepId);
}

/** Display label: role identity first, then title, then the first prompt line. */
export function sessionLabel(s: Session): string {
  const role = s.agent_role?.trim();
  const ordinal = s.epic_spawn_ordinal != null ? ` #${s.epic_spawn_ordinal}` : '';
  const title = s.title?.trim();
  if (role && title) return `${role}${ordinal} · ${title}`;
  if (role) return `${role}${ordinal}`;
  if (title) return title;
  const first = s.query.split('\n').find((l) => l.trim()) ?? '';
  return first.trim().slice(0, 120) || s.id.slice(0, 8);
}

/** Prompt text beyond this is not searched (a pasted blob would only cost memory). */
const QUERY_SEARCH_CHARS = 20000;

const haystacks = new WeakMap<Session, string>();

/** Lower-cased label, provider and prompt, built once per session object so each keystroke is one `includes`. */
function haystack(s: Session): string {
  let h = haystacks.get(s);
  if (h === undefined) {
    h = `${sessionLabel(s)}\n${s.provider}\n${(s.query ?? '').slice(0, QUERY_SEARCH_CHARS)}`.toLowerCase();
    haystacks.set(s, h);
  }
  return h;
}

/**
 * The filter box: the label (role, title or first prompt line), the id prefix, the provider and the
 * prompt text (the TUI searches the prompt too). `needle` is trimmed and lower-cased by the caller.
 */
export function matchesFilterText(s: Session, needle: string): boolean {
  return !needle || haystack(s).includes(needle) || s.id.startsWith(needle);
}

export interface SessionFilter {
  projectId: string | null;
  text: string;
  showFinished: boolean;
  /** The open session: never hidden by the finished filter. */
  keepId?: string | null;
}

/** Filter, then order sessions needing the operator first, then active sessions, and newest update first. */
export function visibleSessions(all: Session[], f: SessionFilter): Session[] {
  const needle = f.text.trim().toLowerCase();
  return all
    .filter((s) => !HIDDEN_BY_DEFAULT.has(s.status))
    .filter((s) => passesFinished(s, f.showFinished, f.keepId))
    .filter((s) => !f.projectId || s.project_id === f.projectId)
    .filter((s) => matchesFilterText(s, needle))
    .sort((a, b) => {
      const need = Number(needsAttention(b)) - Number(needsAttention(a));
      if (need !== 0) return need;
      const act = Number(isActive(b)) - Number(isActive(a));
      if (act !== 0) return act;
      return b.updated_at.localeCompare(a.updated_at);
    });
}

/** Merge a fetched page into the cached events, keyed by sequence. */
export function mergeEvents(cached: ConversationEvent[], fresh: ConversationEvent[]): ConversationEvent[] {
  if (fresh.length === 0) return cached;
  const bySeq = new Map<number, ConversationEvent>();
  for (const e of cached) bySeq.set(e.sequence, e);
  for (const e of fresh) bySeq.set(e.sequence, e);
  return [...bySeq.values()].sort((a, b) => a.sequence - b.sequence);
}

export function maxSequence(events: ConversationEvent[]): number | undefined {
  return events.length ? events[events.length - 1]!.sequence : undefined;
}

export type EventKind = 'user' | 'assistant' | 'tool' | 'result' | 'thinking' | 'system';

export function eventKind(e: ConversationEvent): EventKind {
  switch (e.event_type) {
    case 'Message':
      return e.role === 'User' ? 'user' : 'assistant';
    case 'ToolUse':
      return 'tool';
    case 'ToolResult':
      return 'result';
    case 'Thinking':
      return 'thinking';
    default:
      return 'system';
  }
}

export function relativeTime(iso: string, now: number = Date.now()): string {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '';
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}

const BASE64_RUN = /[A-Za-z0-9+/]{400,}={0,2}/g;
/** Long messages fold behind an expander above this many characters. */
export const FOLD_CHARS = 6000;

/** Replace long base64 runs (inline images, blobs) with a short marker. */
export function elideBlobs(text: string): string {
  return text.replace(BASE64_RUN, (m) => `[${Math.round((m.length * 3) / 4 / 1024)} KiB base64 elided]`);
}
