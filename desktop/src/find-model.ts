// Find in the open conversation (local #307), pure: which events match, which one is current, how many earlier
// events the window must grow to show it, and where the matches sit inside one text. The DOM side is find.ts.
// The match list covers every loaded event, not the rendered window, and never reads the session list filter.
import type { ConversationEvent } from './api.js';
import { isVisible, type Visibility } from './detail-model.js';
import { elideBlobs, eventKind } from './model.js';

/** Events past the match that stay in view when the window grows to reveal it. */
export const REVEAL_CONTEXT = 20;

/** The text of an event as the transcript shows it (blobs elided, tool input as JSON), for matching. */
export function eventSearchText(e: ConversationEvent): string {
  const kind = eventKind(e);
  if (kind === 'tool' || kind === 'result' || kind === 'thinking') {
    let body = e.content;
    if (kind === 'tool' && e.tool_input != null) {
      try { body = JSON.stringify(e.tool_input, null, 2); } catch { /* keep content */ }
    }
    return `${e.tool_name ?? ''}\n${elideBlobs(body)}`;
  }
  return elideBlobs(e.content);
}

/** Lower-cased search text per event object (a long conversation is searched on every keystroke). */
const lowered = new WeakMap<ConversationEvent, string>();

function lowerText(e: ConversationEvent): string {
  let t = lowered.get(e);
  if (t === undefined) { t = eventSearchText(e).toLowerCase(); lowered.set(e, t); }
  return t;
}

/** The needle as typed: a blank one finds nothing, and case never matters. */
export function normalizeNeedle(needle: string): string {
  return needle.trim().toLowerCase();
}

/** Indices (into `events`) of the shown events whose text contains `needle`, in transcript order. */
export function findMatches(events: readonly ConversationEvent[], needle: string, v: Visibility): number[] {
  const n = normalizeNeedle(needle);
  if (n === '') return [];
  const out: number[] = [];
  events.forEach((e, i) => {
    if (isVisible(e, v) && lowerText(e).includes(n)) out.push(i);
  });
  return out;
}

/** The match after (or before) `from`, wrapping; `from` null starts at the first (or last) match. */
export function stepFind(matches: readonly number[], from: number | null, dir: 1 | -1): number | null {
  if (matches.length === 0) return null;
  if (from === null) return dir > 0 ? matches[0]! : matches[matches.length - 1]!;
  if (dir > 0) return matches.find((m) => m > from) ?? matches[0]!;
  for (let i = matches.length - 1; i >= 0; i--) if (matches[i]! < from) return matches[i]!;
  return matches[matches.length - 1]!;
}

/**
 * Where a refreshed match list puts the current match: the same event when it still matches, else the first match
 * at or after it (live events and a changed needle keep the operator near where they were), else the first match.
 */
export function keepCurrent(matches: readonly number[], current: number | null): number | null {
  if (matches.length === 0) return null;
  if (current === null) return matches[0]!;
  return matches.find((m) => m >= current) ?? matches[0]!;
}

/** The count line: "3 of 17", or why there is none. */
export function findLabel(matches: readonly number[], current: number | null, needle: string): string {
  if (normalizeNeedle(needle) === '') return '';
  if (matches.length === 0 || current === null) return 'No match';
  return `${matches.indexOf(current) + 1} of ${matches.length}`;
}

/**
 * The render window (state.renderLimit) that shows event `index` of `total`: unchanged when it is already shown,
 * otherwise grown to include it with a little context before it.
 */
export function limitToReveal(total: number, index: number, limit: number): number {
  const needed = total - index;
  if (needed <= limit) return limit;
  return Math.min(total, needed + REVEAL_CONTEXT);
}

/** [start, end) of each non-overlapping, case-insensitive occurrence of `needle` in `text`. */
export function occurrences(text: string, needle: string): Array<[number, number]> {
  const n = normalizeNeedle(needle);
  if (n === '') return [];
  const hay = text.toLowerCase();
  // toLowerCase can change a string's length (some Unicode capitals); then offsets would not map back.
  if (hay.length !== text.length) return [];
  const out: Array<[number, number]> = [];
  for (let at = hay.indexOf(n); at >= 0; at = hay.indexOf(n, at + n.length)) out.push([at, at + n.length]);
  return out;
}

/** `text` cut at the occurrences of `needle`: alternating plain and matched pieces (matched ones flagged). */
export function splitMatches(text: string, needle: string): Array<{ text: string; hit: boolean }> {
  const out: Array<{ text: string; hit: boolean }> = [];
  let from = 0;
  for (const [a, b] of occurrences(text, needle)) {
    if (a > from) out.push({ text: text.slice(from, a), hit: false });
    out.push({ text: text.slice(a, b), hit: true });
    from = b;
  }
  if (from < text.length) out.push({ text: text.slice(from), hit: false });
  return out;
}

/** What the find chord is, pure (the DOM side calls it from one capture listener). */
export interface FindKey {
  key: string;
  code?: string;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
  isComposing?: boolean;
}

/** Cmd-F on macOS, Ctrl-F elsewhere: works with Vim keys off and from a text field. */
export function isFindChord(ev: FindKey, platform: 'mac' | 'other' | string): boolean {
  if (ev.isComposing || ev.altKey || ev.shiftKey) return false;
  if (ev.key.toLowerCase() !== 'f' && ev.code !== 'KeyF') return false;
  if (platform === 'mac') return !!ev.metaKey && !ev.ctrlKey;
  return !!ev.ctrlKey && !ev.metaKey;
}
