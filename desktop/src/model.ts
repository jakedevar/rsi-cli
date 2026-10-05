// Pure view-model helpers (no DOM), unit-tested under tests/.
import type { ConversationEvent, Session } from './api.js';
import { needsAttention } from './approvals-model.js';

export const ACTIVE_STATUSES = new Set(['Starting', 'Running', 'WaitingApproval']);
export const HIDDEN_BY_DEFAULT = new Set(['Archived', 'Deleted']);

export function isActive(s: Pick<Session, 'status'>): boolean {
  return ACTIVE_STATUSES.has(s.status);
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

export interface SessionFilter {
  projectId: string | null;
  text: string;
  showFinished: boolean;
}

/** Filter, then order sessions needing the operator first, then active sessions, and newest update first. */
export function visibleSessions(all: Session[], f: SessionFilter): Session[] {
  const needle = f.text.trim().toLowerCase();
  return all
    .filter((s) => !HIDDEN_BY_DEFAULT.has(s.status))
    .filter((s) => f.showFinished || isActive(s) || s.status === 'Interrupted' || s.status === 'Failed')
    .filter((s) => !f.projectId || s.project_id === f.projectId)
    .filter((s) =>
      !needle ||
      sessionLabel(s).toLowerCase().includes(needle) ||
      s.id.startsWith(needle) ||
      s.provider.toLowerCase().includes(needle))
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
