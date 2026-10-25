// Pure rules for the session-lifecycle slice (no DOM; unit-tested under
// tests/lifecycle.test.mjs): delete, unarchive and pause preconditions, the
// trash browser, the notification browser's store, the recent-completions list
// and the question modal's key reducer. They mirror the TUI's
// app/session_actions.rs, overlay/trash_browser.rs, overlay/notification_browser.rs,
// overlay/recent_completions.rs and overlay/question_modal.rs.
import type { QuestionItem, Session } from './api.js';
import { type Answer, type PauseLevel, formatAnswers, needsAttention, pendingQuestions } from './approvals-model.js';
import { hintLine } from './key-hints.js';
import type { Platform } from './keyengine.js';
import { sendChord } from './keylabel.js';
import { sessionLabel } from './model.js';

const LIVE = new Set(['Starting', 'Running', 'WaitingApproval']);

// ---------- preconditions ----------

/** Why a session cannot be deleted to the trash now (the TUI's `DD`), else null. */
export function deleteBlock(status: string): string | null {
  if (status === 'Running' || status === 'Starting') return 'Cannot delete an active session. Interrupt it first (x).';
  if (status === 'Archived') return 'The session is archived; unarchive it (U) before deleting.';
  if (status === 'Deleted') return 'The session is already in the trash.';
  return null;
}

/** Why a session cannot be unarchived, else null. */
export function unarchiveBlock(status: string | undefined): string | null {
  return status === 'Archived' ? null : 'No archived session is open (ga lists them).';
}

/** Why a pause cannot be downgraded to SOFT, else null (`<Space>hs`). */
export function downgradeBlock(level: PauseLevel): string | null {
  if (level === 'hard') return null;
  return level === 'soft' ? 'The session is already SOFT paused.' : 'The session has no operator pause marker.';
}

/** The session a lifecycle action acts on: the open one, else the list cursor's. */
export function lifecycleTarget<T extends { id: string }>(open: T | undefined, cursor: T | undefined): T | undefined {
  return open ?? cursor;
}

/** One line for the delete confirmation and the toasts. */
export function previewQuery(s: Pick<Session, 'query' | 'title'>, max = 40): string {
  const t = (s.title?.trim() || s.query).replace(/\s+/g, ' ').trim();
  const chars = [...t];
  return chars.length > max ? `${chars.slice(0, max).join('')}…` : t;
}

// ---------- trash ----------

export type TrashRow = { kind: 'header'; title: string } | { kind: 'session'; session: Session };

const DAY = 86_400_000;

/** The TUI's time-period section for an `updated_at`. */
export function trashGroup(updatedAt: string, now: number): string {
  const age = now - Date.parse(updatedAt);
  if (Number.isNaN(age) || age >= 30 * DAY) return 'Older';
  if (age < DAY) return 'Past Day';
  if (age < 2 * DAY) return 'Past 2 Days';
  if (age < 3 * DAY) return 'Past 3 Days';
  if (age < 7 * DAY) return 'Past Week';
  return 'Past Month';
}

/** Newest first. */
export function sortTrash(sessions: readonly Session[]): Session[] {
  return [...sessions].sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
}

/** Sessions grouped under time-period headers, in list order. */
export function trashRows(sessions: readonly Session[], now: number): TrashRow[] {
  const rows: TrashRow[] = [];
  let group = '';
  for (const session of sessions) {
    const g = trashGroup(session.updated_at, now);
    if (g !== group) { rows.push({ kind: 'header', title: g }); group = g; }
    rows.push({ kind: 'session', session });
  }
  return rows;
}

export const PURGE_CONFIRM_MS = 5000;

export interface TrashState {
  sessions: Session[];
  /** Index into `sessions`. */
  selected: number;
  /** The session a first `D` armed, until a second `D` confirms it. */
  purge: { id: string; at: number } | null;
}

export function openTrash(sessions: readonly Session[]): TrashState {
  return { sessions: sortTrash(sessions), selected: 0, purge: null };
}

export function trashMove(st: TrashState, to: 'next' | 'prev' | 'first' | 'last'): TrashState {
  const last = st.sessions.length - 1;
  if (last < 0) return st;
  const at = to === 'next' ? Math.min(last, st.selected + 1)
    : to === 'prev' ? Math.max(0, st.selected - 1)
      : to === 'first' ? 0 : last;
  return { ...st, selected: at, purge: null };
}

export function trashSelected(st: TrashState): Session | undefined {
  return st.sessions[st.selected];
}

/** Drop a restored or purged session; the selection stays on the same row, clamped. */
export function trashRemove(st: TrashState, id: string): TrashState {
  const sessions = st.sessions.filter((s) => s.id !== id);
  return { sessions, selected: Math.max(0, Math.min(st.selected, sessions.length - 1)), purge: null };
}

/**
 * `D` purges permanently. The TUI purges at once; the desktop asks with a
 * second `D` on the same session within PURGE_CONFIRM_MS (the TUI's own
 * double-tap pattern), because a purge cannot be undone.
 */
export function purgeStep(st: TrashState, now: number): { st: TrashState; run: string | null } {
  const sel = trashSelected(st);
  if (!sel) return { st, run: null };
  if (st.purge && st.purge.id === sel.id && now - st.purge.at < PURGE_CONFIRM_MS) {
    return { st: { ...st, purge: null }, run: sel.id };
  }
  return { st: { ...st, purge: { id: sel.id, at: now } }, run: null };
}

// ---------- notifications ----------

export type NoticeKind = 'info' | 'success' | 'error' | 'completed' | 'failed' | 'attention';

export interface Notice {
  id: number;
  kind: NoticeKind;
  message: string;
  sessionId?: string;
  /** The raw daemon error behind an error notice (shown under Details, copyable). */
  detail?: string;
  at: number;
  ttl: number;
  dismissed: boolean;
  /** When it left the active section by hand. */
  dismissedAt?: number;
}

export interface NoticeStore {
  next: number;
  items: Notice[];
}

export const EMPTY_NOTICES: NoticeStore = { next: 1, items: [] };
/** Oldest history is dropped past this many. */
export const MAX_NOTICES = 200;
/** Same kind on the same session within this window folds into one counted notice. */
export const AGGREGATE_MS = 500;

/** The TUI's TTLs: 10 s for errors, 5 s for the rest. */
export function noticeTtl(kind: NoticeKind): number {
  return kind === 'error' || kind === 'failed' ? 10_000 : 5_000;
}

export function pushNotice(
  store: NoticeStore, n: { kind: NoticeKind; message: string; sessionId?: string; detail?: string }, now: number,
): NoticeStore {
  const last = store.items[store.items.length - 1];
  if (last && !last.dismissed && last.kind === n.kind && last.sessionId === n.sessionId && now - last.at < AGGREGATE_MS) {
    const m = last.message.match(/^\((\d+)\) /);
    const count = m ? Number(m[1]) + 1 : 2;
    const items = store.items.slice(0, -1);
    items.push({ ...last, message: `(${count}) ${n.message}`, at: now, ...(n.detail ? { detail: n.detail } : {}) });
    return { next: store.next, items };
  }
  const note: Notice = {
    id: store.next, kind: n.kind, message: n.message, at: now, ttl: noticeTtl(n.kind), dismissed: false,
    ...(n.sessionId ? { sessionId: n.sessionId } : {}),
    ...(n.detail ? { detail: n.detail } : {}),
  };
  const items = [...store.items, note];
  return { next: store.next + 1, items: items.length > MAX_NOTICES ? items.slice(items.length - MAX_NOTICES) : items };
}

/** Still on the toast / status line: not dismissed and inside its TTL. */
export function noticeActive(n: Notice, now: number): boolean {
  return !n.dismissed && now - n.at < n.ttl;
}

/** Active notices newest first, then history newest first (by when it left the active section). */
export function noticeRows(store: NoticeStore, now: number): Notice[] {
  const active = store.items.filter((n) => noticeActive(n, now)).sort((a, b) => b.at - a.at);
  const retired = (n: Notice): number => n.dismissedAt ?? n.at + n.ttl;
  const history = store.items.filter((n) => !noticeActive(n, now)).sort((a, b) => retired(b) - retired(a));
  return [...active, ...history];
}

export function activeNotices(store: NoticeStore, now: number): number {
  return store.items.filter((n) => noticeActive(n, now)).length;
}

/** `x`: dismiss the notice at display index `index`; history rows cannot be dismissed further. */
export function dismissAt(store: NoticeStore, index: number, now: number): NoticeStore {
  const row = noticeRows(store, now)[index];
  if (!row || !noticeActive(row, now)) return store;
  return { ...store, items: store.items.map((n) => (n.id === row.id ? { ...n, dismissed: true, dismissedAt: now } : n)) };
}

/** `N`: dismiss every active notice. */
export function dismissAll(store: NoticeStore, now: number): NoticeStore {
  return {
    ...store,
    items: store.items.map((n) => (noticeActive(n, now) ? { ...n, dismissed: true, dismissedAt: now } : n)),
  };
}

/** Notice kind for a toast: a success, a failure (the words the desktop's error toasts use), else info. */
export function toastKind(message: string, ok: boolean, detail: string | null = null): NoticeKind {
  if (ok) return 'success';
  if (detail !== null) return 'error';
  return /\b(failed|error|unavailable|could not|cannot|refused)\b/i.test(message) ? 'error' : 'info';
}

/** Session-list changes the notification browser records, by comparing two polls. */
export interface SeenSession { status: string; attention: boolean }
export type SeenMap = ReadonlyMap<string, SeenSession>;

export function sessionNotices(
  seen: SeenMap, sessions: readonly Session[],
): { seen: SeenMap; notices: Array<{ kind: NoticeKind; message: string; sessionId: string }> } {
  const next = new Map<string, SeenSession>();
  const notices: Array<{ kind: NoticeKind; message: string; sessionId: string }> = [];
  for (const s of sessions) {
    const attention = needsAttention(s);
    next.set(s.id, { status: s.status, attention });
    const was = seen.get(s.id);
    if (!was) continue;
    const label = sessionLabel(s);
    if (LIVE.has(was.status) && s.status === 'Completed') notices.push({ kind: 'completed', message: `Completed: ${label}`, sessionId: s.id });
    else if (LIVE.has(was.status) && s.status === 'Failed') notices.push({ kind: 'failed', message: `Failed: ${label}`, sessionId: s.id });
    else if (LIVE.has(was.status) && s.status === 'Interrupted') notices.push({ kind: 'info', message: `Interrupted: ${label}`, sessionId: s.id });
    if (attention && !was.attention) notices.push({ kind: 'attention', message: `Waiting for you: ${label}`, sessionId: s.id });
  }
  return { seen: next, notices };
}

// ---------- recent completions ----------

/** The TUI's recent-completions list: the six latest finished sessions, newest first. */
export function recentCompletions(sessions: readonly Session[], limit = 6): Session[] {
  return sessions
    .filter((s) => s.status === 'Completed' || s.status === 'Failed' || s.status === 'Interrupted')
    .sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at))
    .slice(0, limit);
}

/** Move a list selection one row, clamped (`j`/`k`/`g`/`G`); null for a key that is not a motion. */
export function listKey(index: number, length: number, key: string): number | null {
  if (length <= 0) return null;
  switch (key) {
    case 'j': case 'ArrowDown': return Math.min(length - 1, index + 1);
    case 'k': case 'ArrowUp': return Math.max(0, index - 1);
    case 'g': return 0;
    case 'G': return length - 1;
    default: return null;
  }
}

// ---------- question modal ----------

export interface QuestionState {
  questions: QuestionItem[];
  current: number;
  /** Highlighted option per question. */
  cursor: number[];
  /** Chosen option indexes per question. */
  chosen: number[][];
  /** Free text per question. */
  text: string[];
  mode: 'normal' | 'insert';
}

/** The session whose question the panel opens: the open one if it has one, else the first that does. */
export function questionTarget(sessions: readonly Session[], openId: string | null): Session | undefined {
  const withQuestion = sessions.filter((s) => pendingQuestions(s).length > 0);
  return withQuestion.find((s) => s.id === openId) ?? withQuestion[0];
}

const hasOptions = (q: QuestionItem | undefined): boolean => (q?.options?.length ?? 0) > 0;

export function openQuestion(questions: readonly QuestionItem[]): QuestionState {
  return {
    questions: [...questions],
    current: 0,
    cursor: questions.map(() => 0),
    chosen: questions.map(() => []),
    text: questions.map(() => ''),
    mode: hasOptions(questions[0]) ? 'normal' : 'insert',
  };
}

export interface QuestionKey {
  key: string;
  ctrl?: boolean;
  meta?: boolean;
  shift?: boolean;
  alt?: boolean;
}

export type QuestionEffect = 'submit' | 'decline' | 'hide' | null;

export interface QuestionStep {
  st: QuestionState;
  effect: QuestionEffect;
  /** The key was handled; false leaves it to the text box (typing, Shift-Enter newlines). */
  consumed: boolean;
}

const same = <T,>(arr: readonly T[], at: number, v: T): T[] => arr.map((x, i) => (i === at ? v : x));

function goto(st: QuestionState, current: number): QuestionState {
  return { ...st, current, mode: hasOptions(st.questions[current]) ? 'normal' : 'insert' };
}

/** Pick option `idx` of the current question: single-select replaces, multi-select toggles. Clears the free text. */
export function pickOption(st: QuestionState, idx: number): QuestionState {
  const q = st.questions[st.current];
  if (!q || idx < 0 || idx >= (q.options?.length ?? 0)) return st;
  const now = st.chosen[st.current] ?? [];
  const chosen = q.multiSelect ? (now.includes(idx) ? now.filter((i) => i !== idx) : [...now, idx]) : [idx];
  return {
    ...st,
    cursor: same(st.cursor, st.current, idx),
    chosen: same(st.chosen, st.current, chosen),
    text: same(st.text, st.current, ''),
  };
}

function moveCursor(st: QuestionState, delta: number): QuestionState {
  const q = st.questions[st.current];
  const n = q?.options?.length ?? 0;
  if (!q || n === 0) return st;
  const at = (((st.cursor[st.current] ?? 0) + delta) % n + n) % n;
  // Single-select commits the option as the cursor lands on it (the TUI does too).
  return q.multiSelect ? { ...st, cursor: same(st.cursor, st.current, at) } : pickOption(st, at);
}

/** Free text typed in the box: it replaces any chosen options of that question. */
export function setQuestionText(st: QuestionState, value: string): QuestionState {
  const text = same(st.text, st.current, value);
  return value === '' ? { ...st, text } : { ...st, text, chosen: same(st.chosen, st.current, []) };
}

/** The question modal's keys (docs/keybindings.md "Question / Normal" and "Question / Insert"). */
export function questionKey(st: QuestionState, k: QuestionKey): QuestionStep {
  const ctrl = !!(k.ctrl || k.meta);
  const done = (next: QuestionState, effect: QuestionEffect = null): QuestionStep => ({ st: next, effect, consumed: true });
  if (k.key === 'Enter' && ctrl) return done(st, 'submit');
  if (st.mode === 'insert') {
    if (k.key === 'Escape') return done(hasOptions(st.questions[st.current]) ? { ...st, mode: 'normal' } : st, hasOptions(st.questions[st.current]) ? null : 'hide');
    if (k.key === 'Enter' && !k.shift) return done(st, 'submit');
    return { st, effect: null, consumed: false };
  }
  if (ctrl || k.alt) return { st, effect: null, consumed: false };
  switch (k.key) {
    case 'j': case 'ArrowDown': return done(moveCursor(st, 1));
    case 'k': case 'ArrowUp': return done(moveCursor(st, -1));
    case ' ': return done(pickOption(st, st.cursor[st.current] ?? 0));
    case 'Enter': return done(st.current + 1 < st.questions.length ? goto(st, st.current + 1) : st);
    case 'Backspace': return done(st.current > 0 ? goto(st, st.current - 1) : st);
    case 'i': return done({ ...st, mode: 'insert', chosen: same(st.chosen, st.current, []) });
    case 'd': return done(st, 'decline');
    case 'Escape': case 'q': return done(st, 'hide');
    default:
      if (/^[1-9]$/.test(k.key)) return done(pickOption(st, Number(k.key) - 1));
      return { st, effect: null, consumed: false };
  }
}

export function questionAnswers(st: QuestionState): Answer[] {
  return st.questions.map((_, i) => ({ selected: st.chosen[i] ?? [], text: st.text[i] ?? '' }));
}

/** The `response_text` for AnswerQuestion, or why the answer is incomplete. */
export function questionResponse(st: QuestionState): { ok: true; text: string } | { ok: false; error: string } {
  return formatAnswers(st.questions, questionAnswers(st));
}

/**
 * Footer hint for the question modal's current mode. With Vim keys off the letter keys, 1-9 and
 * Backspace do nothing, so the hint names what works instead: a click or Space on a focused option
 * picks it, the arrows move (and carry focus), Enter goes to the next question (local #246).
 */
export function questionHint(st: QuestionState, vimOn = true, platform?: Platform): string {
  if (st.mode === 'insert') return 'Type an answer. Enter submits, Shift-Enter adds a line, Esc goes back to the options.';
  if (vimOn) return 'j/k move, Space or 1-9 choose, Enter/Backspace next or previous question, i type an answer, Ctrl/Cmd-Enter submits, d lets the agent decide, Esc hides.';
  const more = st.current + 1 < st.questions.length;
  const text = hintLine([
    { do: 'Click an option or press Space on it to pick it' },
    { do: 'Up/Down move between options' },
    { do: 'Tab reaches the answer box and the buttons' },
    ...(more ? [{ do: 'Enter goes to the next question' }] : []),
    { do: `${platform ? sendChord(platform) : 'Ctrl/Cmd-Enter'} submits` },
    { do: 'Esc hides' },
  ], false, platform);
  return `${text}.`;
}
